//! The tools agents use on a channel's artifacts: list them and read any
//! kind; create and change each kind. Writing tools are per kind
//! (`create_table`, `update_document`, …), since each kind's content is
//! shaped differently. Deleting is left to people. Agents get a kind's
//! writing tools when they ask for them ([`USE_ARTIFACT_TOOLS`]).
//!
//! The functions here only change the [`Workspace`]; the app runs the
//! writing ones (see [`is_write`]) itself, to save the artifact's file and
//! show the change in an editor that has it open.

use serde_json::{Value, json};

use crate::agent::tools::channel;
use crate::data::{ArtifactContent, ArtifactKind, Board, BoardColumn, Checklist, ChecklistItem, Document, Table, Workspace};
use crate::whiteboard::{self, Color, Rect, Shape, Whiteboard};
use pots_llm::ToolSpec;

pub const LIST_ARTIFACTS: &str = "list_artifacts";
pub const READ_ARTIFACT: &str = "read_artifact";
pub const CREATE_TABLE: &str = "create_table";
pub const UPDATE_TABLE: &str = "update_table";
pub const ADD_TABLE_ROWS: &str = "add_table_rows";
pub const CREATE_DOCUMENT: &str = "create_document";
pub const UPDATE_DOCUMENT: &str = "update_document";
pub const CREATE_CHECKLIST: &str = "create_checklist";
pub const UPDATE_CHECKLIST: &str = "update_checklist";
pub const ADD_CHECKLIST_ITEMS: &str = "add_checklist_items";
pub const CHECK_ITEMS: &str = "check_items";
pub const CREATE_BOARD: &str = "create_board";
pub const UPDATE_BOARD: &str = "update_board";
pub const ADD_BOARD_CARDS: &str = "add_board_cards";
pub const MOVE_BOARD_CARD: &str = "move_board_card";
pub const CREATE_WHITEBOARD: &str = "create_whiteboard";
pub const ADD_WHITEBOARD_ITEMS: &str = "add_whiteboard_items";
pub const UPDATE_WHITEBOARD_ITEMS: &str = "update_whiteboard_items";
pub const REMOVE_WHITEBOARD_ITEMS: &str = "remove_whiteboard_items";

/// Every artifact tool, in the order agents are offered them.
pub const ALL: [&str; 19] = [
    LIST_ARTIFACTS,
    READ_ARTIFACT,
    CREATE_TABLE,
    UPDATE_TABLE,
    ADD_TABLE_ROWS,
    CREATE_DOCUMENT,
    UPDATE_DOCUMENT,
    CREATE_CHECKLIST,
    UPDATE_CHECKLIST,
    ADD_CHECKLIST_ITEMS,
    CHECK_ITEMS,
    CREATE_BOARD,
    UPDATE_BOARD,
    ADD_BOARD_CARDS,
    MOVE_BOARD_CARD,
    CREATE_WHITEBOARD,
    ADD_WHITEBOARD_ITEMS,
    UPDATE_WHITEBOARD_ITEMS,
    REMOVE_WHITEBOARD_ITEMS,
];

/// The most a table made or changed by an agent may have.
pub const MAX_COLUMNS: usize = 50;
pub const MAX_ROWS: usize = 1000;
/// The longest artifact title, in characters.
pub const MAX_TITLE: usize = 100;
/// The longest document, in characters.
pub const MAX_DOCUMENT: usize = 100_000;
/// The most items a checklist, or cards a board, may have.
pub const MAX_ITEMS: usize = 500;
/// The longest checklist item or board card, in characters.
pub const MAX_ITEM: usize = 2000;
/// The most items a whiteboard may have.
pub const MAX_WHITEBOARD_ITEMS: usize = 1000;

pub fn is_tool(name: &str) -> bool {
    ALL.contains(&name)
}

/// The kind of artifact writing tool `name` changes.
pub fn kind_of(name: &str) -> Option<ArtifactKind> {
    Some(match name {
        CREATE_TABLE | UPDATE_TABLE | ADD_TABLE_ROWS => ArtifactKind::Table,
        CREATE_DOCUMENT | UPDATE_DOCUMENT => ArtifactKind::Document,
        CREATE_CHECKLIST | UPDATE_CHECKLIST | ADD_CHECKLIST_ITEMS | CHECK_ITEMS => ArtifactKind::Checklist,
        CREATE_BOARD | UPDATE_BOARD | ADD_BOARD_CARDS | MOVE_BOARD_CARD => ArtifactKind::Board,
        CREATE_WHITEBOARD | ADD_WHITEBOARD_ITEMS | UPDATE_WHITEBOARD_ITEMS | REMOVE_WHITEBOARD_ITEMS => {
            ArtifactKind::Whiteboard
        }
        _ => return None,
    })
}

/// The tool that offers an agent one kind's writing tools, for the rest
/// of its reply. Agents start with only the reading tools and this one,
/// so most replies don't carry the schemas of every kind (see
/// `docs/artifact-tools-on-demand.md`); [`load`] runs it.
pub const USE_ARTIFACT_TOOLS: &str = "use_artifact_tools";

/// The artifact tools an agent starts a reply with: the reading ones and
/// [`USE_ARTIFACT_TOOLS`].
pub fn offered() -> Vec<ToolSpec> {
    let mut tools: Vec<ToolSpec> = specs().into_iter().filter(|t| !is_write(t.name)).collect();
    let kinds: Vec<&str> = ArtifactKind::ALL.iter().map(|k| k.noun()).collect();
    tools.push(ToolSpec {
        name: USE_ARTIFACT_TOOLS,
        description: "Get the tools to create and change artifacts of these kinds, for the rest of this reply. \
                      table: rows and columns of plain text, like a spreadsheet. document: markdown text (notes, \
                      a plan, a spec). checklist: things to do, ticked off when done. board: a kanban board of \
                      cards in columns. whiteboard: sticky notes, boxes and text placed freely, for brainstorming. \
                      When you'll change an artifact you're about to read, call this in the same step as \
                      read_artifact.",
        input_schema: json!({
            "type": "object",
            "properties": {"kinds": {"type": "array", "minItems": 1, "items": {"type": "string", "enum": kinds}}},
            "required": ["kinds"],
            "additionalProperties": false
        }),
    });
    tools
}

/// Whether `tools` were [offered](offered) with [`USE_ARTIFACT_TOOLS`], so
/// writing tools are added as the agent asks for them.
fn loads_on_demand(tools: &[ToolSpec]) -> bool {
    tools.iter().any(|t| t.name == USE_ARTIFACT_TOOLS)
}

/// Add `kind`'s writing tools to the end of `tools` (keeping what's there,
/// so the request's cached prefix holds): the names added.
fn add_kind(tools: &mut Vec<ToolSpec>, kind: ArtifactKind) -> Vec<&'static str> {
    let new: Vec<ToolSpec> = specs()
        .into_iter()
        .filter(|t| kind_of(t.name) == Some(kind) && !tools.iter().any(|have| have.name == t.name))
        .collect();
    let names = new.iter().map(|t| t.name).collect();
    tools.extend(new);
    names
}

/// Run a [`USE_ARTIFACT_TOOLS`] call: add the kinds it names to `tools`.
/// `None` if `tools` weren't offered it.
pub fn load(tools: &mut Vec<ToolSpec>, input: &Value) -> Option<Result<String, String>> {
    if !loads_on_demand(tools) {
        return None;
    }
    let names: Vec<&str> = match input["kinds"].as_array() {
        Some(names) if !names.is_empty() => names.iter().map(|n| n.as_str().unwrap_or_default()).collect(),
        _ => return Some(Err("`kinds` is required: a list of artifact kinds.".into())),
    };
    // Check every name before adding any, so a failed call adds nothing.
    let mut kinds = Vec::new();
    for name in names {
        let Some(kind) = ArtifactKind::ALL.iter().copied().find(|k| k.noun() == name) else {
            let kinds: Vec<&str> = ArtifactKind::ALL.iter().map(|k| k.noun()).collect();
            return Some(Err(format!("There is no artifact kind “{name}”. Kinds: {}.", kinds.join(", "))));
        };
        kinds.push(kind);
    }
    let mut lines = Vec::new();
    for kind in kinds {
        add_kind(tools, kind);
        let have: Vec<&str> = tools.iter().map(|t| t.name).filter(|n| kind_of(n) == Some(kind)).collect();
        lines.push(format!("You can use the {} tools now: {}.", kind.noun(), have.join(", ")));
    }
    Some(Ok(lines.join("\n")))
}

/// Before running a call to writing tool `name` that the agent wasn't
/// given yet, add that tool's kind to `tools`, so later requests describe
/// the tools the conversation uses.
pub fn load_for(tools: &mut Vec<ToolSpec>, name: &str) {
    if let Some(kind) = kind_of(name).filter(|_| loads_on_demand(tools)) {
        add_kind(tools, kind);
    }
}

/// Give `tools` every writing tool up front, in place of
/// [`USE_ARTIFACT_TOOLS`]: for Claude Code, whose tool list is fixed when
/// it starts.
pub fn load_all(tools: &mut Vec<ToolSpec>) {
    if loads_on_demand(tools) {
        tools.retain(|t| t.name != USE_ARTIFACT_TOOLS);
        let have: Vec<&str> = tools.iter().map(|t| t.name).collect();
        let missing: Vec<ToolSpec> = specs().into_iter().filter(|t| !have.contains(&t.name)).collect();
        tools.extend(missing);
    }
}

/// Whether tool `name` changes an artifact (the rest only read).
pub fn is_write(name: &str) -> bool {
    is_tool(name) && !matches!(name, LIST_ARTIFACTS | READ_ARTIFACT)
}

const ROWS_DESCRIPTION: &str = "The rows, top to bottom: each an array of cell texts, one per column in column \
                                order. Use \"\" for an empty cell.";

pub fn specs() -> Vec<ToolSpec> {
    let rows = json!({"type": "array", "items": {"type": "array", "items": {"type": "string"}}, "description": ROWS_DESCRIPTION});
    let columns = json!({"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Column names, left to right."});
    let id = json!({"type": "integer", "minimum": 0, "description": "The artifact's id, from list_artifacts."});
    let channel = json!({"type": "string", "description": "Channel name, without the #."});
    let title = json!({"type": "string", "description": "A few words saying what it holds."});
    let new_title = json!({"type": "string", "description": "A new title."});
    let text = json!({"type": "string", "description": "The whole text, in markdown."});
    let items = json!({"type": "array", "description": "The items, top to bottom.",
        "items": {"type": "object", "properties": {
            "text": {"type": "string", "description": "What is to be done, in one line."},
            "done": {"type": "boolean", "description": "Whether it is done already. Default false."}
        }, "required": ["text"], "additionalProperties": false}});
    let board_columns = json!({"type": "array", "minItems": 1, "description": "The columns, left to right.",
        "items": {"type": "object", "properties": {
            "name": {"type": "string", "description": "The column's name, like “To do”."},
            "cards": {"type": "array", "items": {"type": "string"}, "description": "Its cards, top to bottom: a line of text each."}
        }, "required": ["name"], "additionalProperties": false}});
    let shapes: Vec<&str> = Shape::ALL.iter().map(|s| s.name()).collect();
    let colors: Vec<&str> = Color::ALL.iter().map(|c| c.name()).collect();
    let item_id = json!({"type": "integer", "minimum": 1, "description": "An item's id, from read_artifact."});
    let wb_items = json!({"type": "array", "minItems": 1, "description": "The items to put on the board.",
        "items": {"type": "object", "properties": {
            "type": {"type": "string", "enum": shapes, "description": "note (a sticky note, the default), box, round_box, or text (no border)."},
            "text": {"type": "string", "description": "What it says. Notes and boxes wrap it; keep it short."},
            "color": {"type": "string", "enum": colors, "description": "Default yellow."},
            "near": {"type": ["integer", "string"], "description": "Put it beside this item: an item's id, or the `key` of an item given earlier in this call. Without `near`, `x` and `y` it goes in the next free place."},
            "key": {"type": "string", "description": "A name for this item, so later items in this call can be put `near` it."},
            "x": {"type": "number", "description": "Left edge, in pixels. Usually leave it out: placing keeps items from overlapping."},
            "y": {"type": "number", "description": "Top edge, in pixels. Give it with `x`."},
            "w": {"type": "number", "description": "Width, in pixels. A note is 160 × 120."},
            "h": {"type": "number", "description": "Height, in pixels."}
        }, "additionalProperties": false}});
    let object = |properties: Value, required: &[&str]| {
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    };
    vec![
        ToolSpec {
            name: LIST_ARTIFACTS,
            description: "List a channel's artifacts (tables, documents, checklists and boards people keep \
                          there, shown as tabs above its messages): each one's id, kind and title.",
            input_schema: object(json!({"channel": channel}), &["channel"]),
        },
        ToolSpec {
            name: READ_ARTIFACT,
            description: "Read an artifact as it is now, including the owner's latest edits: a table's columns \
                          and rows, a document's text, a checklist's items, a board's columns and cards. Read it \
                          again before changing it: people may have edited it since.",
            input_schema: object(json!({"id": id}), &["id"]),
        },
        ToolSpec {
            name: CREATE_TABLE,
            description: "Create a table artifact in a channel: a new tab there that people can open and edit \
                          like a spreadsheet. Cells are plain text. Every row needs exactly one cell per column. \
                          Returns the new table's id.",
            input_schema: object(json!({"channel": channel, "title": title, "columns": columns, "rows": rows}), &["channel", "title", "columns"]),
        },
        ToolSpec {
            name: UPDATE_TABLE,
            description: "Change a table: its title, and/or replace all of its rows (and, with them, its \
                          columns). What you give replaces what's there, so read_artifact first and pass the \
                          rows you keep along with your changes. To only add rows at the bottom, use \
                          add_table_rows instead. Whatever the owner is typing in the table when you change its \
                          cells gives way to your version.",
            input_schema: object(
                json!({
                    "id": id,
                    "title": new_title,
                    "columns": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                                "description": "New column names, left to right. Give `rows` too, shaped to them."},
                    "rows": rows
                }),
                &["id"],
            ),
        },
        ToolSpec {
            name: ADD_TABLE_ROWS,
            description: "Add rows at the bottom of a table, keeping what's there: the way to log entries in a \
                          table as they come.",
            input_schema: object(
                json!({
                    "id": id,
                    "rows": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}, "minItems": 1,
                             "description": ROWS_DESCRIPTION}
                }),
                &["id", "rows"],
            ),
        },
        ToolSpec {
            name: CREATE_DOCUMENT,
            description: "Create a document artifact in a channel: a new tab there holding text in markdown \
                          (notes, a plan, a spec, a write-up) that people can open and edit. Returns its id.",
            input_schema: object(json!({"channel": channel, "title": title, "text": text}), &["channel", "title", "text"]),
        },
        ToolSpec {
            name: UPDATE_DOCUMENT,
            description: "Change a document: its title, and/or replace its whole text. read_artifact first and \
                          give the full new text, the parts you keep included. Whatever the owner is typing in \
                          it gives way to your version.",
            input_schema: object(json!({"id": id, "title": new_title, "text": text}), &["id"]),
        },
        ToolSpec {
            name: CREATE_CHECKLIST,
            description: "Create a checklist artifact in a channel: a new tab there listing things to do, which \
                          people tick off as they're done. Returns its id.",
            input_schema: object(json!({"channel": channel, "title": title, "items": items}), &["channel", "title"]),
        },
        ToolSpec {
            name: UPDATE_CHECKLIST,
            description: "Change a checklist: its title, and/or replace all of its items. What you give replaces \
                          what's there, so read_artifact first and pass the items you keep. To add items at the \
                          bottom use add_checklist_items; to tick items off use check_items.",
            input_schema: object(json!({"id": id, "title": new_title, "items": items}), &["id"]),
        },
        ToolSpec {
            name: ADD_CHECKLIST_ITEMS,
            description: "Add items at the bottom of a checklist, keeping what's there.",
            input_schema: object(json!({"id": id, "items": items}), &["id", "items"]),
        },
        ToolSpec {
            name: CHECK_ITEMS,
            description: "Tick checklist items off as done (or, with done false, back to not done), found by \
                          their text as read_artifact shows it.",
            input_schema: object(
                json!({
                    "id": id,
                    "items": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "The items' texts."},
                    "done": {"type": "boolean", "description": "Default true."}
                }),
                &["id", "items"],
            ),
        },
        ToolSpec {
            name: CREATE_BOARD,
            description: "Create a board artifact in a channel: a kanban board, a new tab there with named \
                          columns (like To do, Doing, Done) of cards that people move along as work goes on. \
                          Without `columns` it has To do, Doing and Done. Returns its id.",
            input_schema: object(json!({"channel": channel, "title": title, "columns": board_columns}), &["channel", "title"]),
        },
        ToolSpec {
            name: UPDATE_BOARD,
            description: "Change a board: its title, and/or replace all of its columns and cards. What you give \
                          replaces what's there, so read_artifact first and pass what you keep. To add cards use \
                          add_board_cards; to move one use move_board_card.",
            input_schema: object(json!({"id": id, "title": new_title, "columns": board_columns}), &["id"]),
        },
        ToolSpec {
            name: ADD_BOARD_CARDS,
            description: "Add cards at the bottom of one of a board's columns, keeping what's there.",
            input_schema: object(
                json!({
                    "id": id,
                    "column": {"type": "string", "description": "The column's name."},
                    "cards": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "A line of text each."}
                }),
                &["id", "column", "cards"],
            ),
        },
        ToolSpec {
            name: MOVE_BOARD_CARD,
            description: "Move a card on a board to the bottom of another column (from Doing to Done, say), \
                          found by its text as read_artifact shows it.",
            input_schema: object(
                json!({
                    "id": id,
                    "card": {"type": "string", "description": "The card's text."},
                    "to": {"type": "string", "description": "The name of the column it goes to."}
                }),
                &["id", "card", "to"],
            ),
        },
        ToolSpec {
            name: CREATE_WHITEBOARD,
            description: "Create a whiteboard artifact in a channel: a new tab there, an endless board of sticky \
                          notes, boxes and text that people move around freely, for brainstorming, sketching a \
                          plan or grouping ideas. Items you give are placed so they don't overlap. Returns its id \
                          and the items' ids.",
            input_schema: object(json!({"channel": channel, "title": title, "items": wb_items}), &["channel", "title"]),
        },
        ToolSpec {
            name: ADD_WHITEBOARD_ITEMS,
            description: "Add notes, boxes or text to a whiteboard, keeping what's there. Group related ideas \
                          with `near` (beside an item) and color, rather than coordinates. Returns the new items' ids.",
            input_schema: object(json!({"id": id, "items": wb_items}), &["id", "items"]),
        },
        ToolSpec {
            name: UPDATE_WHITEBOARD_ITEMS,
            description: "Change a whiteboard: its title, and/or items on it, by their ids from read_artifact: \
                          what they say, their color or type, where they are, or their size. Only what you give \
                          changes.",
            input_schema: object(
                json!({
                    "id": id,
                    "title": new_title,
                    "changes": {"type": "array", "minItems": 1, "items": {"type": "object", "properties": {
                        "item": item_id,
                        "text": {"type": "string"},
                        "color": {"type": "string", "enum": colors},
                        "type": {"type": "string", "enum": shapes},
                        "x": {"type": "number"}, "y": {"type": "number"},
                        "w": {"type": "number"}, "h": {"type": "number"}
                    }, "required": ["item"], "additionalProperties": false}}
                }),
                &["id"],
            ),
        },
        ToolSpec {
            name: REMOVE_WHITEBOARD_ITEMS,
            description: "Take items off a whiteboard, by their ids from read_artifact.",
            input_schema: object(
                json!({"id": id, "items": {"type": "array", "minItems": 1, "items": item_id, "description": "The items' ids."}}),
                &["id", "items"],
            ),
        },
    ]
}

/// What the UI shows while an artifact tool runs.
pub fn activity(name: &str, input: &Value) -> Option<String> {
    if name == USE_ARTIFACT_TOOLS {
        let kinds: Vec<&str> = input["kinds"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        return Some(match kinds.as_slice() {
            [] => "Getting artifact tools…".to_string(),
            kinds => format!("Getting {} tools…", kinds.join(" and ")),
        });
    }
    let channel = input["channel"].as_str().map(|c| c.trim_start_matches('#'));
    let id = input["id"].as_u64();
    let creating = |noun: &str, c: &str| match input["title"].as_str() {
        Some(t) => format!("Creating {noun} “{}” in #{c}…", t.trim()),
        None => format!("Creating a {noun} in #{c}…"),
    };
    Some(match (name, channel, id) {
        (LIST_ARTIFACTS, Some(c), _) => format!("Looking at #{c}'s artifacts…"),
        (READ_ARTIFACT, _, Some(id)) => format!("Reading artifact {id}…"),
        (CREATE_TABLE, Some(c), _) => creating("table", c),
        (CREATE_DOCUMENT, Some(c), _) => creating("document", c),
        (CREATE_CHECKLIST, Some(c), _) => creating("checklist", c),
        (CREATE_BOARD, Some(c), _) => creating("board", c),
        (CREATE_WHITEBOARD, Some(c), _) => creating("whiteboard", c),
        (UPDATE_TABLE, _, Some(id)) => format!("Updating table {id}…"),
        (UPDATE_DOCUMENT, _, Some(id)) => format!("Updating document {id}…"),
        (UPDATE_CHECKLIST, _, Some(id)) => format!("Updating checklist {id}…"),
        (UPDATE_BOARD, _, Some(id)) => format!("Updating board {id}…"),
        (ADD_TABLE_ROWS, _, Some(id)) => format!("Adding rows to table {id}…"),
        (ADD_CHECKLIST_ITEMS, _, Some(id)) => format!("Adding items to checklist {id}…"),
        (CHECK_ITEMS, _, Some(id)) => format!("Ticking items in checklist {id}…"),
        (ADD_BOARD_CARDS, _, Some(id)) => format!("Adding cards to board {id}…"),
        (MOVE_BOARD_CARD, _, Some(id)) => format!("Moving a card on board {id}…"),
        (ADD_WHITEBOARD_ITEMS, _, Some(id)) => format!("Adding to whiteboard {id}…"),
        (UPDATE_WHITEBOARD_ITEMS, _, Some(id)) => format!("Changing whiteboard {id}…"),
        (REMOVE_WHITEBOARD_ITEMS, _, Some(id)) => format!("Taking items off whiteboard {id}…"),
        _ => return None,
    })
}

/// Run a reading tool ([`LIST_ARTIFACTS`] or [`READ_ARTIFACT`]).
pub fn read(ws: &Workspace, name: &str, input: &Value) -> Result<String, String> {
    match name {
        LIST_ARTIFACTS => list(ws, input),
        READ_ARTIFACT => read_artifact(ws, input),
        _ => Err(format!("There is no tool named {name}.")),
    }
}

/// What a writing tool did.
#[derive(Debug, PartialEq, Eq)]
pub struct Changed {
    pub id: usize,
    /// Whether the content changed (not only the title).
    pub content: bool,
    /// Whether the title changed.
    pub title: bool,
    /// A line for the owner, under the agent's reply.
    pub shown: String,
    /// The tool result for the agent.
    pub result: String,
}

/// Run a writing tool (see [`is_write`]) on the workspace.
pub fn write(ws: &mut Workspace, name: &str, input: &Value) -> Result<Changed, String> {
    match name {
        CREATE_TABLE => create_table(ws, input),
        UPDATE_TABLE => update_table(ws, input),
        ADD_TABLE_ROWS => add_table_rows(ws, input),
        CREATE_DOCUMENT => create_document(ws, input),
        UPDATE_DOCUMENT => update_document(ws, input),
        CREATE_CHECKLIST => create_checklist(ws, input),
        UPDATE_CHECKLIST => update_checklist(ws, input),
        ADD_CHECKLIST_ITEMS => add_checklist_items(ws, input),
        CHECK_ITEMS => check_items(ws, input),
        CREATE_BOARD => create_board(ws, input),
        UPDATE_BOARD => update_board(ws, input),
        ADD_BOARD_CARDS => add_board_cards(ws, input),
        MOVE_BOARD_CARD => move_board_card(ws, input),
        CREATE_WHITEBOARD => create_whiteboard(ws, input),
        ADD_WHITEBOARD_ITEMS => add_whiteboard_items(ws, input),
        UPDATE_WHITEBOARD_ITEMS => update_whiteboard_items(ws, input),
        REMOVE_WHITEBOARD_ITEMS => remove_whiteboard_items(ws, input),
        _ => Err(format!("There is no tool named {name}.")),
    }
}

fn list(ws: &Workspace, input: &Value) -> Result<String, String> {
    let c = channel(ws, input)?;
    let ids = ws.channel_artifacts(c);
    if ids.is_empty() {
        return Ok(format!("#{} has no artifacts.", ws.channels[c].name));
    }
    let mut out = format!("Artifacts in #{}:\n", ws.channels[c].name);
    for id in ids {
        let a = ws.artifact(id).expect("listed");
        out.push_str(&format!("- id {id}: {} “{}”", a.kind.noun(), a.title));
        match &a.content {
            Some(content) => out.push_str(&format!(" ({})", size(content))),
            None => out.push_str(" (its file can't be read)"),
        }
        out.push('\n');
    }
    Ok(out)
}

fn read_artifact(ws: &Workspace, input: &Value) -> Result<String, String> {
    let id = artifact_id(ws, input)?;
    let a = ws.artifact(id).expect("checked");
    let content = readable(ws, id)?;
    let head = format!("{} {id} “{}” in #{}, {}", a.kind.label(), a.title, ws.channels[a.channel].name, size(content));
    Ok(match content {
        ArtifactContent::Table(t) => format!("{head}:\n{}", json!({"columns": t.columns, "rows": t.rows})),
        ArtifactContent::Document(d) if d.text.trim().is_empty() => format!("{head}."),
        ArtifactContent::Document(d) => format!("{head}:\n{}", d.text),
        ArtifactContent::Checklist(c) => format!("{head}:\n{}", json!({"items": c.items})),
        ArtifactContent::Board(b) => format!("{head}:\n{}", json!({"columns": b.columns})),
        ArtifactContent::Whiteboard(b) if b.items.is_empty() => format!("{head}."),
        ArtifactContent::Whiteboard(b) => format!(
            "{head}, first drawn first (later ones lie on top). x and y are each item's top left, in pixels:\n{}",
            json!({"items": b.items.iter().map(item_json).collect::<Vec<_>>()})
        ),
    })
}

// --- creating and changing, for every kind --------------------------------

/// A new artifact holding `content`, in the channel and with the title `input` gives.
fn create(ws: &mut Workspace, input: &Value, content: ArtifactContent) -> Result<Changed, String> {
    let c = channel(ws, input)?;
    let title = title(&input["title"])?.ok_or("`title` is required.")?;
    title_free(ws, c, &title, None)?;
    let noun = content.kind().noun();
    let size = size(&content);
    let id = ws.add_artifact_with(c, title.clone(), content);
    let place = &ws.channels[c].name;
    Ok(Changed {
        id,
        content: true,
        title: true,
        shown: format!("Created {noun} “{title}” in #{place} ({size})."),
        result: format!(
            "Created {noun} {id} “{title}” in #{place} ({size}). It is a tab in #{place} now; people can edit it \
             too, so read it again before changing it later."
        ),
    })
}

/// Give artifact `id` the title and/or content given (`None`: kept), and
/// say what changed.
fn update(ws: &mut Workspace, id: usize, new_title: Option<String>, new_content: Option<ArtifactContent>) -> Result<Changed, String> {
    if let Some(t) = &new_title {
        title_free(ws, ws.artifact(id).expect("checked").channel, t, Some(id))?;
    }
    let a = ws.artifact_mut(id).expect("checked");
    let kind = a.kind;
    let content = new_content.as_ref().is_some_and(|c| Some(c) != a.content.as_ref());
    let title = new_title.as_ref().is_some_and(|t| *t != a.title);
    if let Some(t) = new_title.filter(|_| title) {
        a.title = t;
    }
    if let Some(c) = new_content.filter(|_| content) {
        a.content = Some(c);
    }
    let a = ws.artifact(id).expect("checked");
    let (name, place) = (a.title.clone(), ws.channels[a.channel].name.clone());
    let size = a.content.as_ref().map(size).unwrap_or_default();
    let (label, noun) = (kind.label(), kind.noun());
    let what = match (title, content) {
        (true, true) => "renamed and rewrote",
        (true, false) => "renamed",
        (false, true) => "rewrote",
        (false, false) => {
            return Ok(Changed {
                id,
                content,
                title,
                shown: String::new(),
                result: format!("{label} {id} “{name}” already says that; nothing changed."),
            });
        }
    };
    let what_cap = format!("{}{}", what[..1].to_uppercase(), &what[1..]);
    Ok(Changed {
        id,
        content,
        title,
        shown: format!("{what_cap} {noun} “{name}” in #{place} ({size})."),
        result: format!("{label} {id} “{name}” in #{place} is {what} ({size})."),
    })
}

/// Artifact `id` now holds `content`, changed by `done` ("Added 2 rows
/// to"), said of it.
fn changed(ws: &mut Workspace, id: usize, content: ArtifactContent, done: &str) -> Changed {
    let size = size(&content);
    let a = ws.artifact_mut(id).expect("checked");
    a.content = Some(content);
    let a = ws.artifact(id).expect("checked");
    let (noun, name, place) = (a.kind.noun(), a.title.clone(), ws.channels[a.channel].name.clone());
    Changed {
        id,
        content: true,
        title: false,
        shown: format!("{done} {noun} “{name}” in #{place} ({size})."),
        result: format!("{done} {noun} {id} “{name}” in #{place}; it has {size} now."),
    }
}

/// An error if another artifact in channel `c` than `except` already has
/// `title` (ignoring case): two tabs of the same name are hard to tell
/// apart, and a retried create would make a second one.
fn title_free(ws: &Workspace, c: usize, title: &str, except: Option<usize>) -> Result<(), String> {
    match ws.artifacts.iter().find(|a| a.channel == c && Some(a.id) != except && a.title.eq_ignore_ascii_case(title)) {
        Some(a) => Err(format!(
            "#{} already has a {} titled “{}” (id {}). Change it with {}, or pick another title.",
            ws.channels[c].name,
            a.kind.noun(),
            a.title,
            a.id,
            update_tool(a.kind)
        )),
        None => Ok(()),
    }
}

// --- tables ------------------------------------------------------------------

fn create_table(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    channel(ws, input)?;
    let columns = columns(&input["columns"])?.ok_or("`columns` is required: the column names, left to right.")?;
    let rows = match &input["rows"] {
        Value::Null => Vec::new(),
        v => rows(v, &columns, 0)?,
    };
    create(ws, input, ArtifactContent::Table(Table { columns, rows }))
}

fn update_table(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let old = table(ws, id)?;
    let new_title = title(&input["title"])?;
    let new_columns = columns(&input["columns"])?;
    let new_rows = match (&input["rows"], &new_columns) {
        (Value::Null, Some(_)) => {
            return Err("Give `rows` with `columns`: the rows are replaced too, one cell per new column.".into());
        }
        (Value::Null, None) => None,
        (v, Some(columns)) => Some(rows(v, columns, 0)?),
        (v, None) => Some(rows(v, &old.columns, 0)?),
    };
    if new_title.is_none() && new_rows.is_none() {
        return Err("Nothing to change: give `title`, `rows`, or `columns` with `rows`.".into());
    }
    let content = new_rows.map(|rows| {
        ArtifactContent::Table(Table { columns: new_columns.unwrap_or_else(|| old.columns.clone()), rows })
    });
    update(ws, id, new_title, content)
}

fn add_table_rows(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let old = table(ws, id)?;
    let added = rows(&input["rows"], &old.columns, old.rows.len())?;
    if added.is_empty() {
        return Err("`rows` is empty: give at least one row to add.".into());
    }
    let mut table = old.clone();
    let n = added.len();
    table.rows.extend(added);
    Ok(changed(ws, id, ArtifactContent::Table(table), &format!("Added {} to", plural(n, "row"))))
}

// --- documents ---------------------------------------------------------------

fn create_document(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    channel(ws, input)?;
    let text = document_text(&input["text"])?.ok_or("`text` is required: the document's text, in markdown.")?;
    create(ws, input, ArtifactContent::Document(Document { text }))
}

fn update_document(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    of_kind(ws, id, ArtifactKind::Document)?;
    let new_title = title(&input["title"])?;
    let text = document_text(&input["text"])?;
    if new_title.is_none() && text.is_none() {
        return Err("Nothing to change: give `title` and/or `text`.".into());
    }
    update(ws, id, new_title, text.map(|text| ArtifactContent::Document(Document { text })))
}

fn document_text(v: &Value) -> Result<Option<String>, String> {
    let text = match v {
        Value::Null => return Ok(None),
        v => v.as_str().ok_or("`text` must be text (a string), in markdown.")?,
    };
    if text.chars().count() > MAX_DOCUMENT {
        return Err(format!("`text` is too long: a document has at most {MAX_DOCUMENT} characters."));
    }
    Ok(Some(text.to_string()))
}

// --- checklists --------------------------------------------------------------

fn create_checklist(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    channel(ws, input)?;
    let items = match &input["items"] {
        Value::Null => Vec::new(),
        v => items(v, 0)?,
    };
    create(ws, input, ArtifactContent::Checklist(Checklist { items }))
}

fn update_checklist(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    of_kind(ws, id, ArtifactKind::Checklist)?;
    let new_title = title(&input["title"])?;
    let new_items = match &input["items"] {
        Value::Null => None,
        v => Some(items(v, 0)?),
    };
    if new_title.is_none() && new_items.is_none() {
        return Err("Nothing to change: give `title` and/or `items`.".into());
    }
    update(ws, id, new_title, new_items.map(|items| ArtifactContent::Checklist(Checklist { items })))
}

fn add_checklist_items(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut list = checklist(ws, id)?.clone();
    let added = items(&input["items"], list.items.len())?;
    if added.is_empty() {
        return Err("`items` is empty: give at least one item to add.".into());
    }
    let n = added.len();
    list.items.extend(added);
    Ok(changed(ws, id, ArtifactContent::Checklist(list), &format!("Added {} to", plural(n, "item"))))
}

fn check_items(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut list = checklist(ws, id)?.clone();
    let done = match &input["done"] {
        Value::Null => true,
        v => v.as_bool().ok_or("`done` must be true or false.")?,
    };
    let wanted = texts(&input["items"], "items", "the items' texts")?;
    if wanted.is_empty() {
        return Err("`items` is empty: give the texts of the items to tick.".into());
    }
    let mut found = Vec::new();
    for want in &wanted {
        let at: Vec<usize> = (0..list.items.len()).filter(|&i| same_text(&list.items[i].text, want)).collect();
        if at.is_empty() {
            let have: Vec<&str> = list.items.iter().map(|i| i.text.as_str()).collect();
            return Err(format!(
                "No item says “{want}”. Nothing was changed. The items are: {}.",
                if have.is_empty() { "none".to_string() } else { have.iter().map(|t| format!("“{t}”")).collect::<Vec<_>>().join(", ") }
            ));
        }
        found.extend(at);
    }
    let mut n = 0;
    for i in found {
        if list.items[i].done != done {
            list.items[i].done = done;
            n += 1;
        }
    }
    let a = ws.artifact(id).expect("checked");
    if n == 0 {
        return Ok(Changed {
            id,
            content: false,
            title: false,
            shown: String::new(),
            result: format!("Checklist {id} “{}”: already so; nothing changed.", a.title),
        });
    }
    let how = if done { "Ticked off" } else { "Unticked" };
    Ok(changed(ws, id, ArtifactContent::Checklist(list), &format!("{how} {} in", plural(n, "item"))))
}

/// Checklist items from `v`, for a checklist that has `before` items besides them.
/// An item is `{"text": …, "done": …}`, or only its text.
fn items(v: &Value, before: usize) -> Result<Vec<ChecklistItem>, String> {
    let list = v.as_array().ok_or("`items` must be an array of items, each {\"text\": …, \"done\": …}.")?;
    if before + list.len() > MAX_ITEMS {
        return Err(format!("That makes {} items, too many: a checklist has at most {MAX_ITEMS}.", before + list.len()));
    }
    list.iter()
        .enumerate()
        .map(|(i, item)| {
            let (text, done) = match item {
                Value::String(s) => (Some(s.as_str()), Value::Null),
                Value::Object(o) => (o.get("text").and_then(Value::as_str), o.get("done").cloned().unwrap_or(Value::Null)),
                _ => (None, Value::Null),
            };
            let text = text.ok_or_else(|| format!("Item {} has no `text`: give each item as {{\"text\": …}}.", i + 1))?;
            let done = match done {
                Value::Null => false,
                v => v.as_bool().ok_or_else(|| format!("Item {}: `done` must be true or false.", i + 1))?,
            };
            Ok(ChecklistItem { text: line(text, &format!("Item {}", i + 1))?, done })
        })
        .collect()
}

// --- boards ------------------------------------------------------------------

fn create_board(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    channel(ws, input)?;
    let board = match &input["columns"] {
        Value::Null => Board::blank(),
        v => board_columns(v)?,
    };
    create(ws, input, ArtifactContent::Board(board))
}

fn update_board(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    of_kind(ws, id, ArtifactKind::Board)?;
    let new_title = title(&input["title"])?;
    let new_board = match &input["columns"] {
        Value::Null => None,
        v => Some(board_columns(v)?),
    };
    if new_title.is_none() && new_board.is_none() {
        return Err("Nothing to change: give `title` and/or `columns`.".into());
    }
    update(ws, id, new_title, new_board.map(ArtifactContent::Board))
}

fn add_board_cards(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut board = board(ws, id)?.clone();
    let col = board_column(&board, &input["column"], "column")?;
    let cards = cards(&input["cards"], board.cards())?;
    if cards.is_empty() {
        return Err("`cards` is empty: give at least one card to add.".into());
    }
    let n = cards.len();
    board.columns[col].cards.extend(cards);
    let done = format!("Added {} to “{}” on", plural(n, "card"), board.columns[col].name);
    Ok(changed(ws, id, ArtifactContent::Board(board), &done))
}

fn move_board_card(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut board = board(ws, id)?.clone();
    let want = input["card"].as_str().ok_or("`card` is required: the card's text.")?;
    let to = board_column(&board, &input["to"], "to")?;
    let at = board
        .columns
        .iter()
        .enumerate()
        .find_map(|(c, col)| col.cards.iter().position(|card| same_text(card, want)).map(|i| (c, i)));
    let Some((col, ix)) = at else {
        return Err(format!("No card on the board says “{}”. Nothing was changed. read_artifact shows its cards.", want.trim()));
    };
    if col == to {
        return Ok(Changed {
            id,
            content: false,
            title: false,
            shown: String::new(),
            result: format!("That card is in “{}” already; nothing changed.", board.columns[to].name),
        });
    }
    let card = board.columns[col].cards[ix].clone();
    board.move_card(col, ix, to);
    let done = format!("Moved “{card}” from “{}” to “{}” on", board.columns[col].name, board.columns[to].name);
    Ok(changed(ws, id, ArtifactContent::Board(board), &done))
}

/// A board from `[{"name": …, "cards": [...]}, ...]`.
fn board_columns(v: &Value) -> Result<Board, String> {
    let list = v.as_array().ok_or("`columns` must be an array of columns, each {\"name\": …, \"cards\": [...]}.")?;
    if list.is_empty() {
        return Err("`columns` is empty: a board needs at least one column.".into());
    }
    if list.len() > MAX_COLUMNS {
        return Err(format!("{} columns is too many: at most {MAX_COLUMNS}.", list.len()));
    }
    let mut board = Board::default();
    for (i, col) in list.iter().enumerate() {
        let name = col["name"].as_str().map(str::trim).unwrap_or_default();
        if name.is_empty() {
            return Err(format!("Column {} has no name: every column needs one.", i + 1));
        }
        let cards = match &col["cards"] {
            Value::Null => Vec::new(),
            v => cards(v, board.cards())?,
        };
        board.columns.push(BoardColumn { name: name.to_string(), cards });
    }
    Ok(board)
}

/// Card texts from `v`, for a board that has `before` cards besides them.
fn cards(v: &Value, before: usize) -> Result<Vec<String>, String> {
    let list = texts(v, "cards", "a line of text each")?;
    if before + list.len() > MAX_ITEMS {
        return Err(format!("That makes {} cards, too many: a board has at most {MAX_ITEMS}.", before + list.len()));
    }
    list.iter().enumerate().map(|(i, t)| line(t, &format!("Card {}", i + 1))).collect()
}

/// The index of the column `v` names (ignoring case), for field `field`.
fn board_column(board: &Board, v: &Value, field: &str) -> Result<usize, String> {
    let name = v.as_str().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| format!("`{field}` is required: a column's name."))?;
    board.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
        let names: Vec<String> = board.columns.iter().map(|c| format!("“{}”", c.name)).collect();
        format!("The board has no column “{name}”. Its columns are {}.", names.join(", "))
    })
}

// --- whiteboards -------------------------------------------------------------

fn create_whiteboard(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    channel(ws, input)?;
    let mut board = Whiteboard::default();
    let ids = match &input["items"] {
        Value::Null => Vec::new(),
        v => add_items(&mut board, v)?,
    };
    let mut made = create(ws, input, ArtifactContent::Whiteboard(board))?;
    made.result.push_str(&ids_said(&ids));
    Ok(made)
}

fn add_whiteboard_items(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut board = whiteboard_of(ws, id)?.clone();
    let ids = add_items(&mut board, &input["items"])?;
    let mut c = changed(ws, id, ArtifactContent::Whiteboard(board), &format!("Added {} to", plural(ids.len(), "item")));
    c.result.push_str(&ids_said(&ids));
    Ok(c)
}

fn update_whiteboard_items(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut board = whiteboard_of(ws, id)?.clone();
    let new_title = title(&input["title"])?;
    let list = match (&input["changes"], &new_title) {
        (Value::Null, Some(_)) => return update(ws, id, new_title, None),
        (v, _) => v.as_array().ok_or("`changes` must be an array: {\"item\": id, …what changes} each.")?,
    };
    if list.is_empty() {
        return Err("`changes` is empty: give at least one item to change.".into());
    }
    for (n, change) in list.iter().enumerate() {
        let what = format!("Change {}", n + 1);
        let item = change["item"].as_u64().ok_or_else(|| format!("{what} has no `item`: an item's id, from read_artifact."))?;
        let shape = shape_of(&change["type"], &what)?;
        let color = color_of(&change["color"], &what)?;
        let text = text_of(&change["text"], &what)?;
        let num = |k: &str| number(&change[k], &format!("{what}: `{k}`"));
        let (x, y, w, h) = (num("x")?, num("y")?, num("w")?, num("h")?);
        let i = board.item_mut(item).ok_or_else(|| format!("{what}: the board has no item {item}. Nothing was changed. read_artifact shows its items."))?;
        if let Some(shape) = shape {
            i.shape = shape;
        }
        if let Some(color) = color {
            i.color = color;
        }
        if let Some(text) = text {
            i.text = text;
        }
        i.x = x.map_or(i.x, whiteboard::clamp_coord);
        i.y = y.map_or(i.y, whiteboard::clamp_coord);
        (i.w, i.h) = whiteboard::clamp_size(w.unwrap_or(i.w), h.unwrap_or(i.h));
        if i.shape == Shape::Text && i.text.trim().is_empty() {
            return Err(format!("{what}: text with nothing in it would be invisible. Remove it with remove_whiteboard_items instead."));
        }
    }
    if new_title.is_some() {
        return update(ws, id, new_title, Some(ArtifactContent::Whiteboard(board)));
    }
    if Some(&ArtifactContent::Whiteboard(board.clone())) == ws.artifact(id).expect("checked").content.as_ref() {
        let title = ws.artifact(id).expect("checked").title.clone();
        return Ok(Changed { id, content: false, title: false, shown: String::new(), result: format!("Whiteboard {id} “{title}” already says that; nothing changed.") });
    }
    Ok(changed(ws, id, ArtifactContent::Whiteboard(board), &format!("Changed {} on", plural(list.len(), "item"))))
}

fn remove_whiteboard_items(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let id = artifact_id(ws, input)?;
    let mut board = whiteboard_of(ws, id)?.clone();
    let list = input["items"].as_array().ok_or("`items` must be an array of item ids, from read_artifact.")?;
    if list.is_empty() {
        return Err("`items` is empty: give the ids of the items to take off.".into());
    }
    for v in list {
        let item = v.as_u64().ok_or("`items` holds item ids: numbers, from read_artifact.")?;
        if !board.remove(item) {
            return Err(format!("The board has no item {item}. Nothing was changed. read_artifact shows its items."));
        }
    }
    Ok(changed(ws, id, ArtifactContent::Whiteboard(board), &format!("Took {} off", plural(list.len(), "item"))))
}

/// Put the items `v` describes on `board`, placing those without `x` and
/// `y`. Returns their ids, in order.
fn add_items(board: &mut Whiteboard, v: &Value) -> Result<Vec<u64>, String> {
    let list = v.as_array().ok_or("`items` must be an array of items, each {\"type\": …, \"text\": …}.")?;
    if list.is_empty() {
        return Err("`items` is empty: give at least one item to add.".into());
    }
    if board.items.len() + list.len() > MAX_WHITEBOARD_ITEMS {
        return Err(format!(
            "That makes {} items, too many: a whiteboard has at most {MAX_WHITEBOARD_ITEMS}.",
            board.items.len() + list.len()
        ));
    }
    let mut keys: Vec<(String, u64)> = Vec::new();
    let mut ids = Vec::new();
    for (n, item) in list.iter().enumerate() {
        let what = format!("Item {}", n + 1);
        if !item.is_object() {
            return Err(format!("{what} isn't an object: give each item as {{\"type\": …, \"text\": …}}."));
        }
        let shape = shape_of(&item["type"], &what)?.unwrap_or_default();
        let color = color_of(&item["color"], &what)?.unwrap_or_default();
        let text = text_of(&item["text"], &what)?.unwrap_or_default();
        if shape == Shape::Text && text.trim().is_empty() {
            return Err(format!("{what} is text with nothing in it: give it `text`."));
        }
        let (dw, dh) = shape.default_size();
        let num = |k: &str| number(&item[k], &format!("{what}: `{k}`"));
        let (w, h) = whiteboard::clamp_size(num("w")?.unwrap_or(dw), num("h")?.unwrap_or(dh));
        let near = match &item["near"] {
            Value::Null => None,
            Value::String(k) => Some(keys.iter().find(|(key, _)| key == k.trim()).map(|&(_, id)| id).ok_or_else(|| {
                format!("{what}: `near` is “{k}”, but no item before it in this call has that `key`.")
            })?),
            v => {
                let near = v.as_u64().ok_or_else(|| format!("{what}: `near` is an item's id, or an earlier item's `key`."))?;
                board.item(near).ok_or_else(|| format!("{what}: `near` is {near}, but the board has no item {near}."))?;
                Some(near)
            }
        };
        let (x, y) = match (num("x")?, num("y")?) {
            (Some(x), Some(y)) => (x, y),
            (None, None) => board.place(w, h, near),
            _ => return Err(format!("{what}: give `x` and `y` together, or neither.")),
        };
        let id = board.add_sized(shape, Rect { x, y, w, h }, &text, color);
        if let Some(key) = item["key"].as_str() {
            keys.push((key.trim().to_string(), id));
        }
        ids.push(id);
    }
    Ok(ids)
}

/// " New items' ids: 4, 5, 6."
fn ids_said(ids: &[u64]) -> String {
    match ids {
        [] => String::new(),
        [id] => format!(" The new item's id: {id}."),
        ids => format!(" New items' ids, in the order given: {}.", ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")),
    }
}

/// An item as read_artifact shows it: whole pixels, no color on text.
fn item_json(i: &whiteboard::WhiteboardItem) -> Value {
    let mut v = json!({"id": i.id, "type": i.shape.name(), "text": i.text,
        "x": i.x.round() as i64, "y": i.y.round() as i64, "w": i.w.round() as i64, "h": i.h.round() as i64});
    if i.shape != Shape::Text {
        v["color"] = json!(i.color.name());
    }
    v
}

fn shape_of(v: &Value, what: &str) -> Result<Option<Shape>, String> {
    match v {
        Value::Null => Ok(None),
        v => v.as_str().and_then(Shape::from_name).map(Some).ok_or_else(|| {
            let names: Vec<&str> = Shape::ALL.iter().map(|s| s.name()).collect();
            format!("{what}: `type` is one of {}.", names.join(", "))
        }),
    }
}

fn color_of(v: &Value, what: &str) -> Result<Option<Color>, String> {
    match v {
        Value::Null => Ok(None),
        v => v.as_str().and_then(Color::from_name).map(Some).ok_or_else(|| {
            let names: Vec<&str> = Color::ALL.iter().map(|c| c.name()).collect();
            format!("{what}: `color` is one of {}.", names.join(", "))
        }),
    }
}

fn text_of(v: &Value, what: &str) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.chars().count() > MAX_ITEM => Err(format!("{what}: `text` is too long: keep it to {MAX_ITEM} characters.")),
        Value::String(s) => Ok(Some(s.trim().to_string())),
        _ => Err(format!("{what}: `text` must be text (a string).")),
    }
}

fn number(v: &Value, what: &str) -> Result<Option<f32>, String> {
    match v {
        Value::Null => Ok(None),
        v => v.as_f64().map(|n| Some(n as f32)).ok_or_else(|| format!("{what} must be a number.")),
    }
}

// --- checking what the agent passed ----------------------------------------

/// "3 columns × 2 rows", "12 lines", "2 of 5 done", "3 columns, 7 cards".
fn size(content: &ArtifactContent) -> String {
    match content {
        ArtifactContent::Table(t) => format!("{} × {}", plural(t.columns.len(), "column"), plural(t.rows.len(), "row")),
        ArtifactContent::Document(d) if d.text.trim().is_empty() => "empty".to_string(),
        ArtifactContent::Document(d) => plural(d.lines(), "line"),
        ArtifactContent::Checklist(c) if c.items.is_empty() => "no items".to_string(),
        ArtifactContent::Checklist(c) => format!("{} of {} done", c.done(), c.items.len()),
        ArtifactContent::Board(b) => format!("{}, {}", plural(b.columns.len(), "column"), plural(b.cards(), "card")),
        ArtifactContent::Whiteboard(b) if b.items.is_empty() => "empty".to_string(),
        ArtifactContent::Whiteboard(b) => plural(b.items.len(), "item"),
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// Whether `have` is what `want` says, ignoring case and surrounding space.
fn same_text(have: &str, want: &str) -> bool {
    have.trim().to_lowercase() == want.trim().to_lowercase()
}

/// A checklist item's or card's text: one line, not blank.
fn line(text: &str, what: &str) -> Result<String, String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err(format!("{what} is blank: give it a few words."));
    }
    if text.chars().count() > MAX_ITEM {
        return Err(format!("{what} is too long: keep it to {MAX_ITEM} characters."));
    }
    Ok(text)
}

/// The strings in array `v`, field `field`.
fn texts(v: &Value, field: &str, what: &str) -> Result<Vec<String>, String> {
    let list = v.as_array().ok_or_else(|| format!("`{field}` must be an array of strings: {what}."))?;
    list.iter()
        .enumerate()
        .map(|(i, t)| t.as_str().map(str::to_string).ok_or_else(|| format!("`{field}` item {} isn't text: give strings.", i + 1)))
        .collect()
}

/// The id in `input["id"]`, of an artifact that exists.
fn artifact_id(ws: &Workspace, input: &Value) -> Result<usize, String> {
    let id = match &input["id"] {
        Value::Null => return Err("`id` is required: an artifact's id, from list_artifacts.".into()),
        v => v.as_u64().ok_or("`id` must be a number, from list_artifacts.")? as usize,
    };
    match ws.artifact(id) {
        Some(_) => Ok(id),
        None => Err(format!("There is no artifact {id}. list_artifacts shows a channel's artifacts and their ids.")),
    }
}

/// Artifact `id`'s content, unless its file couldn't be read.
fn readable(ws: &Workspace, id: usize) -> Result<&ArtifactContent, String> {
    let a = ws.artifact(id).expect("checked");
    a.content.as_ref().ok_or_else(|| {
        format!(
            "Artifact {id} “{}” can't be used: its file couldn't be read ({}). Only the owner can fix or delete it.",
            a.title,
            a.load_error.as_deref().unwrap_or("unknown error")
        )
    })
}

/// Artifact `id`'s content, for the tools that change artifacts of `kind`:
/// an error if it is another kind.
fn of_kind(ws: &Workspace, id: usize, kind: ArtifactKind) -> Result<&ArtifactContent, String> {
    let content = readable(ws, id)?;
    match content.kind() {
        k if k == kind => Ok(content),
        k => Err(format!(
            "Artifact {id} “{}” is a {}, not a {}. Use {} and its other tools for it.",
            ws.artifact(id).expect("checked").title,
            k.noun(),
            kind.noun(),
            update_tool(k)
        )),
    }
}

/// The tool that changes artifacts of `kind`.
fn update_tool(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Table => UPDATE_TABLE,
        ArtifactKind::Document => UPDATE_DOCUMENT,
        ArtifactKind::Checklist => UPDATE_CHECKLIST,
        ArtifactKind::Board => UPDATE_BOARD,
        ArtifactKind::Whiteboard => UPDATE_WHITEBOARD_ITEMS,
    }
}

fn table(ws: &Workspace, id: usize) -> Result<&Table, String> {
    match of_kind(ws, id, ArtifactKind::Table)? {
        ArtifactContent::Table(t) => Ok(t),
        _ => unreachable!("checked"),
    }
}

fn checklist(ws: &Workspace, id: usize) -> Result<&Checklist, String> {
    match of_kind(ws, id, ArtifactKind::Checklist)? {
        ArtifactContent::Checklist(c) => Ok(c),
        _ => unreachable!("checked"),
    }
}

fn whiteboard_of(ws: &Workspace, id: usize) -> Result<&Whiteboard, String> {
    match of_kind(ws, id, ArtifactKind::Whiteboard)? {
        ArtifactContent::Whiteboard(b) => Ok(b),
        _ => unreachable!("checked"),
    }
}

fn board(ws: &Workspace, id: usize) -> Result<&Board, String> {
    match of_kind(ws, id, ArtifactKind::Board)? {
        ArtifactContent::Board(b) => Ok(b),
        _ => unreachable!("checked"),
    }
}

/// A title: whitespace collapsed; `None` if not given.
fn title(v: &Value) -> Result<Option<String>, String> {
    let title = match v {
        Value::Null => return Ok(None),
        v => v.as_str().ok_or("`title` must be text.")?,
    };
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err("`title` is blank: give it a few words saying what it holds.".into());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!("`title` is too long: keep it to {MAX_TITLE} characters."));
    }
    Ok(Some(title))
}

/// Column names; `None` if not given.
fn columns(v: &Value) -> Result<Option<Vec<String>>, String> {
    let list = match v {
        Value::Null => return Ok(None),
        v => v.as_array().ok_or("`columns` must be an array of column names.")?,
    };
    if list.is_empty() {
        return Err("`columns` is empty: a table needs at least one column.".into());
    }
    if list.len() > MAX_COLUMNS {
        return Err(format!("{} columns is too many: at most {MAX_COLUMNS}.", list.len()));
    }
    list.iter()
        .enumerate()
        .map(|(i, c)| match c.as_str().map(str::trim) {
            Some("") => Err(format!("Column {} has no name: every column needs one.", i + 1)),
            Some(name) => Ok(name.to_string()),
            None => Err(format!("Column {} isn't text: column names are strings.", i + 1)),
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

/// Rows shaped to `columns`, for a table that has `before` rows besides them.
fn rows(v: &Value, columns: &[String], before: usize) -> Result<Vec<Vec<String>>, String> {
    let list = v.as_array().ok_or("`rows` must be an array of rows, each an array of cell texts.")?;
    if before + list.len() > MAX_ROWS {
        return Err(format!("That makes {} rows, too many: a table has at most {MAX_ROWS}.", before + list.len()));
    }
    list.iter()
        .enumerate()
        .map(|(r, row)| {
            let cells = row.as_array().ok_or_else(|| format!("Row {} isn't an array: give each row as an array of cell texts.", r + 1))?;
            if cells.len() != columns.len() {
                return Err(format!(
                    "Row {} has {} cells, but the table has {} columns ({}). Give every row one cell per column, \
                     \"\" for an empty one. Nothing was changed.",
                    r + 1,
                    cells.len(),
                    columns.len(),
                    columns.join(", ")
                ));
            }
            cells
                .iter()
                .enumerate()
                .map(|(c, cell)| match cell {
                    Value::String(s) => Ok(s.clone()),
                    // Plainly meant as text: 3 is "3".
                    Value::Number(n) => Ok(n.to_string()),
                    Value::Bool(b) => Ok(b.to_string()),
                    _ => Err(format!("Row {}, column “{}”: a cell is text (a string), not {cell}.", r + 1, columns[c])),
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools;
    use crate::data;

    fn table_of(ws: &Workspace, id: usize) -> &Table {
        match ws.artifact(id).unwrap().content.as_ref().unwrap() {
            ArtifactContent::Table(t) => t,
            other => panic!("not a table: {other:?}"),
        }
    }

    fn content_of(ws: &Workspace, id: usize) -> &ArtifactContent {
        ws.artifact(id).unwrap().content.as_ref().unwrap()
    }

    #[test]
    fn a_document_is_created_read_and_rewritten() {
        let mut ws = data::seed();
        let made = write(&mut ws, CREATE_DOCUMENT, &json!({"channel": "general", "title": "Launch plan",
            "text": "# Plan\n\n- ship it\n"})).unwrap();
        assert_eq!(made.shown, "Created document “Launch plan” in #general (3 lines).");
        assert_eq!(*content_of(&ws, made.id), ArtifactContent::Document(Document { text: "# Plan\n\n- ship it\n".into() }));
        let listed = read(&ws, LIST_ARTIFACTS, &json!({"channel": "general"})).unwrap();
        assert!(listed.contains(&format!("- id {}: document “Launch plan” (3 lines)", made.id)), "{listed}");
        let shown = read(&ws, READ_ARTIFACT, &json!({"id": made.id})).unwrap();
        assert_eq!(shown, format!("Document {} “Launch plan” in #general, 3 lines:\n# Plan\n\n- ship it\n", made.id));

        let c = write(&mut ws, UPDATE_DOCUMENT, &json!({"id": made.id, "text": "Done."})).unwrap();
        assert!(c.content && !c.title);
        assert_eq!(c.shown, "Rewrote document “Launch plan” in #general (1 line).");
        let c = write(&mut ws, UPDATE_DOCUMENT, &json!({"id": made.id, "text": "Done."})).unwrap();
        assert!(!c.content && c.shown.is_empty(), "same text changes nothing");

        let empty = write(&mut ws, CREATE_DOCUMENT, &json!({"channel": "general", "title": "Blank", "text": ""})).unwrap();
        assert_eq!(read(&ws, READ_ARTIFACT, &json!({"id": empty.id})).unwrap(), format!("Document {} “Blank” in #general, empty.", empty.id));

        for (name, input, want) in [
            (CREATE_DOCUMENT, json!({"channel": "general", "title": "X"}), "`text` is required"),
            (CREATE_DOCUMENT, json!({"channel": "general", "title": "X", "text": 3}), "`text` must be text"),
            (CREATE_DOCUMENT, json!({"channel": "general", "title": "X", "text": "x".repeat(MAX_DOCUMENT + 1)}), "too long"),
            (UPDATE_DOCUMENT, json!({"id": made.id}), "Nothing to change"),
        ] {
            let e = err(write(&mut ws, name, &input));
            assert!(e.contains(want), "{name} {input}: {e}");
        }
    }

    #[test]
    fn a_checklist_is_created_added_to_and_ticked_off() {
        let mut ws = data::seed();
        let made = write(&mut ws, CREATE_CHECKLIST, &json!({"channel": "general", "title": "Release",
            "items": [{"text": "Write notes"}, {"text": " Tag  v1 ", "done": true}, "Tweet"]})).unwrap();
        assert_eq!(made.shown, "Created checklist “Release” in #general (1 of 3 done).");
        let id = made.id;
        let ArtifactContent::Checklist(list) = content_of(&ws, id) else { panic!() };
        assert_eq!(list.items[1], ChecklistItem { text: "Tag v1".into(), done: true }, "texts are tidied");

        let shown = read(&ws, READ_ARTIFACT, &json!({"id": id})).unwrap();
        let (head, body) = shown.split_once('\n').unwrap();
        assert_eq!(head, format!("Checklist {id} “Release” in #general, 1 of 3 done:"));
        assert_eq!(serde_json::from_str::<Value>(body).unwrap()["items"][2], json!({"text": "Tweet", "done": false}));

        let c = write(&mut ws, ADD_CHECKLIST_ITEMS, &json!({"id": id, "items": [{"text": "Blog post"}]})).unwrap();
        assert_eq!(c.shown, "Added 1 item to checklist “Release” in #general (1 of 4 done).");

        let c = write(&mut ws, CHECK_ITEMS, &json!({"id": id, "items": ["write notes", "Tag v1"]})).unwrap();
        assert_eq!(c.shown, "Ticked off 1 item in checklist “Release” in #general (2 of 4 done).", "Tag v1 was done already");
        let c = write(&mut ws, CHECK_ITEMS, &json!({"id": id, "items": ["Tag v1"], "done": false})).unwrap();
        assert_eq!(c.shown, "Unticked 1 item in checklist “Release” in #general (1 of 4 done).");
        let c = write(&mut ws, CHECK_ITEMS, &json!({"id": id, "items": ["Tag v1"], "done": false})).unwrap();
        assert!(!c.content && c.shown.is_empty());

        let before = content_of(&ws, id).clone();
        for (name, input, want) in [
            (CHECK_ITEMS, json!({"id": id, "items": ["Write notes", "Nope"]}), "No item says “Nope”. Nothing was changed. The items are: “Write notes”"),
            (CHECK_ITEMS, json!({"id": id, "items": []}), "`items` is empty"),
            (ADD_CHECKLIST_ITEMS, json!({"id": id, "items": [{"text": "  "}]}), "Item 1 is blank"),
            (ADD_CHECKLIST_ITEMS, json!({"id": id, "items": [{"done": true}]}), "Item 1 has no `text`"),
            (ADD_CHECKLIST_ITEMS, json!({"id": id, "items": [{"text": "a", "done": "yes"}]}), "`done` must be true or false"),
            (ADD_CHECKLIST_ITEMS, json!({"id": id, "items": vec![json!("x"); MAX_ITEMS]}), "too many"),
            (UPDATE_CHECKLIST, json!({"id": id}), "Nothing to change"),
        ] {
            let e = err(write(&mut ws, name, &input));
            assert!(e.contains(want), "{name} {input}: {e}");
        }
        assert_eq!(*content_of(&ws, id), before, "a call that failed changed nothing");

        let c = write(&mut ws, UPDATE_CHECKLIST, &json!({"id": id, "title": "Ship", "items": []})).unwrap();
        assert!(c.content && c.title);
        assert_eq!(c.shown, "Renamed and rewrote checklist “Ship” in #general (no items).");
    }

    #[test]
    fn a_board_is_created_and_its_cards_added_and_moved() {
        let mut ws = data::seed();
        let made = write(&mut ws, CREATE_BOARD, &json!({"channel": "general", "title": "Sprint"})).unwrap();
        assert_eq!(made.shown, "Created board “Sprint” in #general (3 columns, 0 cards).");
        let id = made.id;

        let c = write(&mut ws, ADD_BOARD_CARDS, &json!({"id": id, "column": "to do", "cards": ["Login", "Signup"]})).unwrap();
        assert_eq!(c.shown, "Added 2 cards to “To do” on board “Sprint” in #general (3 columns, 2 cards).");
        let c = write(&mut ws, MOVE_BOARD_CARD, &json!({"id": id, "card": "login", "to": "Doing"})).unwrap();
        assert_eq!(c.shown, "Moved “Login” from “To do” to “Doing” on board “Sprint” in #general (3 columns, 2 cards).");
        let ArtifactContent::Board(b) = content_of(&ws, id) else { panic!() };
        assert_eq!((b.columns[0].cards.as_slice(), b.columns[1].cards.as_slice()), (&["Signup".to_string()][..], &["Login".to_string()][..]));
        let c = write(&mut ws, MOVE_BOARD_CARD, &json!({"id": id, "card": "Login", "to": "doing"})).unwrap();
        assert!(!c.content, "already there");

        let shown = read(&ws, READ_ARTIFACT, &json!({"id": id})).unwrap();
        let (head, body) = shown.split_once('\n').unwrap();
        assert_eq!(head, format!("Board {id} “Sprint” in #general, 3 columns, 2 cards:"));
        assert_eq!(serde_json::from_str::<Value>(body).unwrap()["columns"][1], json!({"name": "Doing", "cards": ["Login"]}));

        let before = content_of(&ws, id).clone();
        for (name, input, want) in [
            (ADD_BOARD_CARDS, json!({"id": id, "column": "Later", "cards": ["x"]}), "The board has no column “Later”. Its columns are “To do”, “Doing”, “Done”."),
            (ADD_BOARD_CARDS, json!({"id": id, "column": "Done", "cards": []}), "`cards` is empty"),
            (ADD_BOARD_CARDS, json!({"id": id, "cards": ["x"]}), "`column` is required"),
            (MOVE_BOARD_CARD, json!({"id": id, "card": "Nope", "to": "Done"}), "No card on the board says “Nope”"),
            (UPDATE_BOARD, json!({"id": id, "columns": []}), "at least one column"),
            (UPDATE_BOARD, json!({"id": id, "columns": [{"name": " "}]}), "Column 1 has no name"),
            (UPDATE_BOARD, json!({"id": id, "columns": [{"name": "A", "cards": [""]}]}), "Card 1 is blank"),
        ] {
            let e = err(write(&mut ws, name, &input));
            assert!(e.contains(want), "{name} {input}: {e}");
        }
        assert_eq!(*content_of(&ws, id), before, "a call that failed changed nothing");

        let c = write(&mut ws, UPDATE_BOARD, &json!({"id": id, "columns": [{"name": "Backlog", "cards": ["A", "B"]}, {"name": "Live"}]})).unwrap();
        assert_eq!(c.shown, "Rewrote board “Sprint” in #general (2 columns, 2 cards).");
    }

    fn whiteboard_of_ws(ws: &Workspace, id: usize) -> &Whiteboard {
        match content_of(ws, id) {
            ArtifactContent::Whiteboard(b) => b,
            other => panic!("not a whiteboard: {other:?}"),
        }
    }

    #[test]
    fn a_whiteboard_is_made_with_items_placed_beside_each_other() {
        let mut ws = data::seed();
        let made = write(&mut ws, CREATE_WHITEBOARD, &json!({"channel": "general", "title": "Ideas", "items": [
            {"text": "Pros", "type": "box", "color": "green", "key": "pros"},
            {"text": "Fast", "near": "pros"},
            {"text": "Cons", "type": "box", "color": "pink", "x": 0, "y": 400},
            {"type": "text", "text": "Q4 plan", "x": 0, "y": -80, "w": 300}
        ]})).unwrap();
        let id = made.id;
        assert_eq!(made.shown, "Created whiteboard “Ideas” in #general (4 items).");
        assert!(made.result.ends_with("New items' ids, in the order given: 1, 2, 3, 4."), "{}", made.result);
        let b = whiteboard_of_ws(&ws, id);
        let (pros, fast, cons) = (b.item(1).unwrap(), b.item(2).unwrap(), b.item(3).unwrap());
        assert_eq!((pros.shape, pros.color), (Shape::Box, Color::Green));
        assert_eq!((fast.x, fast.y), (pros.x + pros.w + whiteboard::GAP, pros.y), "beside the item it's near");
        assert_eq!((cons.x, cons.y), (0., 400.));
        assert_eq!(b.item(4).unwrap().w, 300.);

        let shown = read(&ws, READ_ARTIFACT, &json!({"id": id})).unwrap();
        let (head, body) = shown.split_once('\n').unwrap();
        assert!(head.starts_with(&format!("Whiteboard {id} “Ideas” in #general, 4 items, first drawn first")), "{head}");
        let items = &serde_json::from_str::<Value>(body).unwrap()["items"];
        assert_eq!(items[0], json!({"id": 1, "type": "box", "text": "Pros", "color": "green", "x": 0, "y": 0, "w": 180, "h": 90}));
        assert!(items[3].get("color").is_none(), "text has no color: {}", items[3]);

        let c = write(&mut ws, ADD_WHITEBOARD_ITEMS, &json!({"id": id, "items": [{"text": "Cheap", "near": 2}]})).unwrap();
        assert_eq!(c.shown, "Added 1 item to whiteboard “Ideas” in #general (5 items).");
        assert!(c.result.ends_with("The new item's id: 5."), "{}", c.result);
        let b = whiteboard_of_ws(&ws, id);
        assert!(b.items.iter().filter(|i| i.id != 5).all(|i| !i.rect().near(&b.item(5).unwrap().rect(), 1.)), "it lands on no other");
    }

    #[test]
    fn whiteboard_items_are_changed_and_removed_by_id() {
        let mut ws = data::seed();
        let id = write(&mut ws, CREATE_WHITEBOARD, &json!({"channel": "general", "title": "Map",
            "items": [{"text": "a"}, {"text": "b"}]})).unwrap().id;
        let c = write(&mut ws, UPDATE_WHITEBOARD_ITEMS, &json!({"id": id, "changes": [
            {"item": 1, "text": "A!", "color": "blue", "x": 500},
            {"item": 2, "type": "round_box", "w": 10}
        ]})).unwrap();
        assert_eq!(c.shown, "Changed 2 items on whiteboard “Map” in #general (2 items).");
        let b = whiteboard_of_ws(&ws, id);
        let (a, bb) = (b.item(1).unwrap(), b.item(2).unwrap());
        assert_eq!((a.text.as_str(), a.color, a.x, a.y), ("A!", Color::Blue, 500., 0.), "only what's given changes");
        assert_eq!((bb.shape, bb.w), (Shape::RoundBox, whiteboard::MIN_W), "sizes are kept within bounds");
        let c = write(&mut ws, UPDATE_WHITEBOARD_ITEMS, &json!({"id": id, "changes": [{"item": 1, "text": "A!"}]})).unwrap();
        assert!(!c.content && c.shown.is_empty(), "same text changes nothing");
        let c = write(&mut ws, UPDATE_WHITEBOARD_ITEMS, &json!({"id": id, "title": "Mind map"})).unwrap();
        assert!(c.title && !c.content);

        let before = content_of(&ws, id).clone();
        for (name, input, want) in [
            (UPDATE_WHITEBOARD_ITEMS, json!({"id": id, "changes": [{"item": 1, "text": "x"}, {"item": 9, "text": "y"}]}), "Change 2: the board has no item 9. Nothing was changed."),
            (UPDATE_WHITEBOARD_ITEMS, json!({"id": id, "changes": [{"item": 1, "color": "teal"}]}), "`color` is one of yellow, blue, green, pink, purple, gray"),
            (UPDATE_WHITEBOARD_ITEMS, json!({"id": id, "changes": [{"item": 1, "type": "text", "text": " "}]}), "would be invisible"),
            (UPDATE_WHITEBOARD_ITEMS, json!({"id": id}), "`changes` must be an array"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": [{"type": "circle"}]}), "`type` is one of note, box, round_box, text"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": [{"text": "x", "x": 3}]}), "give `x` and `y` together"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": [{"text": "x", "near": "nope"}]}), "no item before it in this call has that `key`"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": [{"text": "x", "near": 77}]}), "the board has no item 77"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": [{"type": "text"}]}), "text with nothing in it"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": [{"text": "x", "w": "big"}]}), "`w` must be a number"),
            (ADD_WHITEBOARD_ITEMS, json!({"id": id, "items": vec![json!({"text": "x"}); MAX_WHITEBOARD_ITEMS]}), "too many"),
            (REMOVE_WHITEBOARD_ITEMS, json!({"id": id, "items": [1, 5]}), "The board has no item 5. Nothing was changed."),
            (REMOVE_WHITEBOARD_ITEMS, json!({"id": id, "items": []}), "`items` is empty"),
        ] {
            let e = err(write(&mut ws, name, &input));
            assert!(e.contains(want), "{name} {input}: {e}");
        }
        assert_eq!(*content_of(&ws, id), before, "a call that failed changed nothing");

        let c = write(&mut ws, REMOVE_WHITEBOARD_ITEMS, &json!({"id": id, "items": [2]})).unwrap();
        assert_eq!(c.shown, "Took 1 item off whiteboard “Mind map” in #general (1 item).");
        let c = write(&mut ws, ADD_WHITEBOARD_ITEMS, &json!({"id": id, "items": [{"text": "c"}]})).unwrap();
        assert!(c.result.ends_with("id: 3."), "ids aren't reused: {}", c.result);
    }

    #[test]
    fn each_kinds_tools_refuse_the_other_kinds() {
        let mut ws = data::seed();
        let doc = write(&mut ws, CREATE_DOCUMENT, &json!({"channel": "general", "title": "Notes", "text": "hi"})).unwrap().id;
        let e = err(write(&mut ws, ADD_TABLE_ROWS, &json!({"id": doc, "rows": [["x"]]})));
        assert_eq!(e, format!("Artifact {doc} “Notes” is a document, not a table. Use update_document and its other tools for it."));
        for name in [UPDATE_TABLE, UPDATE_CHECKLIST, CHECK_ITEMS, UPDATE_BOARD, MOVE_BOARD_CARD] {
            let e = err(write(&mut ws, name, &json!({"id": doc, "title": "X", "items": ["hi"], "card": "hi", "to": "Done"})));
            assert!(e.contains("is a document, not a"), "{name}: {e}");
        }
        // Titles stay apart across kinds, and the error names the right tool.
        let e = err(write(&mut ws, CREATE_BOARD, &json!({"channel": "general", "title": "notes"})));
        assert_eq!(e, format!("#general already has a document titled “Notes” (id {doc}). Change it with update_document, or pick another title."));
    }

    fn err(r: Result<impl std::fmt::Debug, String>) -> String {
        r.expect_err("should fail")
    }

    #[test]
    fn a_table_is_created_listed_and_read() {
        let mut ws = data::seed();
        let general = ws.find_channel("general").unwrap();
        let made = write(&mut ws, CREATE_TABLE, &json!({"channel": "general", "title": "Expenses Q4",
            "columns": ["Item", "Cost"], "rows": [["Coffee", "4.5"], ["Desk", 120]]})).unwrap();
        assert!(made.content && made.title);
        assert_eq!(made.shown, "Created table “Expenses Q4” in #general (2 columns × 2 rows).");
        assert!(made.result.contains(&format!("table {} “Expenses Q4”", made.id)), "{}", made.result);
        let a = ws.artifact(made.id).unwrap();
        assert_eq!((a.channel, a.kind, a.title.as_str()), (general, data::ArtifactKind::Table, "Expenses Q4"));
        assert_eq!(table_of(&ws, made.id).rows, [["Coffee", "4.5"], ["Desk", "120"]], "numbers become text");

        let listed = read(&ws, LIST_ARTIFACTS, &json!({"channel": "general"})).unwrap();
        assert!(listed.contains(&format!("- id {}: table “Expenses Q4” (2 columns × 2 rows)", made.id)), "{listed}");
        assert_eq!(read(&ws, LIST_ARTIFACTS, &json!({"channel": "random"})).unwrap(), "#random has no artifacts.");
        let (channels, _) = tools::run(&ws, "list_channels", &json!({}));
        assert!(channels.contains(&format!("  table {} “Expenses Q4”", made.id)), "{channels}");

        let shown = read(&ws, READ_ARTIFACT, &json!({"id": made.id})).unwrap();
        let (head, body) = shown.split_once('\n').unwrap();
        assert_eq!(head, format!("Table {} “Expenses Q4” in #general, 2 columns × 2 rows:", made.id));
        let back: Value = serde_json::from_str(body).unwrap();
        assert_eq!(back, json!({"columns": ["Item", "Cost"], "rows": [["Coffee", "4.5"], ["Desk", "120"]]}));
        // Through the shared entry point too, as agents reach it.
        assert_eq!(tools::run(&ws, READ_ARTIFACT, &json!({"id": made.id})), (shown, false));

        // Titles and column names are tidied.
        let tidy = write(&mut ws, CREATE_TABLE, &json!({"channel": "#general", "title": "  Lunch \n spots ", "columns": [" Place "]})).unwrap();
        assert_eq!(ws.artifact(tidy.id).unwrap().title, "Lunch spots");
        assert_eq!(table_of(&ws, tidy.id).columns, ["Place"]);
    }

    #[test]
    fn creating_checks_what_it_is_given() {
        let mut ws = data::seed();
        let ok = json!({"channel": "general", "title": "T", "columns": ["A", "B"], "rows": [["1", "2"]]});
        let with = |k: &str, v: Value| {
            let mut input = ok.clone();
            input[k] = v;
            input
        };
        let cases = [
            (with("channel", json!("nope")), "No channel named nope. Channels:"),
            (with("title", json!("  ")), "`title` is blank"),
            (with("title", json!("x".repeat(MAX_TITLE + 1))), "too long"),
            (with("columns", json!([])), "at least one column"),
            (with("columns", json!(["A", " "])), "Column 2 has no name"),
            (with("columns", json!("A, B")), "`columns` must be an array"),
            (with("rows", json!([["1", "2"], ["3"]])), "Row 2 has 1 cells, but the table has 2 columns (A, B)"),
            (with("rows", json!([["1", "2", "3"]])), "Row 1 has 3 cells"),
            (with("rows", json!(["1", "2"])), "Row 1 isn't an array"),
            (with("rows", json!([["1", null]])), "Row 1, column “B”: a cell is text"),
            (with("rows", json!(vec![json!(["", ""]); MAX_ROWS + 1])), "too many"),
            (json!({"channel": "general", "title": "T"}), "`columns` is required"),
        ];
        for (input, want) in cases {
            let e = err(write(&mut ws, CREATE_TABLE, &input));
            assert!(e.contains(want), "{input}: {e}");
        }
        assert!(ws.artifacts.is_empty(), "nothing made by a call that failed");

        let id = write(&mut ws, CREATE_TABLE, &ok).unwrap().id;
        let e = err(write(&mut ws, CREATE_TABLE, &with("title", json!("t"))));
        assert_eq!(e, format!("#general already has a table titled “T” (id {id}). Change it with update_table, or pick another title."));
        assert!(write(&mut ws, CREATE_TABLE, &with("channel", json!("random"))).is_ok(), "the same title elsewhere is fine");
        // Without rows, the table starts empty.
        let empty = write(&mut ws, CREATE_TABLE, &json!({"channel": "general", "title": "Log", "columns": ["When"]})).unwrap();
        assert!(table_of(&ws, empty.id).rows.is_empty());
    }

    #[test]
    fn a_table_is_rewritten_renamed_and_added_to() {
        let mut ws = data::seed();
        let id = write(&mut ws, CREATE_TABLE, &json!({"channel": "general", "title": "Expenses",
            "columns": ["Item", "Cost"], "rows": [["Coffee", "4.5"]]})).unwrap().id;

        // Rows only: the columns stay.
        let c = write(&mut ws, UPDATE_TABLE, &json!({"id": id, "rows": [["Tea", "3"], ["Desk", "120"]]})).unwrap();
        assert!(c.content && !c.title);
        assert_eq!(c.shown, "Rewrote table “Expenses” in #general (2 columns × 2 rows).");
        assert_eq!(table_of(&ws, id).columns, ["Item", "Cost"]);
        assert_eq!(table_of(&ws, id).rows, [["Tea", "3"], ["Desk", "120"]]);

        // Title only: the content stays.
        let c = write(&mut ws, UPDATE_TABLE, &json!({"id": id, "title": "Q4  spend"})).unwrap();
        assert!(!c.content && c.title);
        assert_eq!(c.shown, "Renamed table “Q4 spend” in #general (2 columns × 2 rows).");
        assert_eq!(ws.artifact(id).unwrap().title, "Q4 spend");

        // Everything at once.
        let c = write(&mut ws, UPDATE_TABLE, &json!({"id": id, "title": "Spend", "columns": ["What", "€", "Paid"],
            "rows": [["Tea", "3", "yes"]]})).unwrap();
        assert!(c.content && c.title);
        assert_eq!(*table_of(&ws, id), Table { columns: vec!["What".into(), "€".into(), "Paid".into()], rows: vec![vec!["Tea".into(), "3".into(), "yes".into()]] });

        // Saying what's there already changes nothing.
        let c = write(&mut ws, UPDATE_TABLE, &json!({"id": id, "title": "Spend", "rows": [["Tea", "3", "yes"]]})).unwrap();
        assert!(!c.content && !c.title && c.shown.is_empty(), "{c:?}");

        let c = write(&mut ws, ADD_TABLE_ROWS, &json!({"id": id, "rows": [["Pens", "2", "no"], ["Ink", "9", ""]]})).unwrap();
        assert!(c.content && !c.title);
        assert_eq!(c.shown, "Added 2 rows to table “Spend” in #general (3 columns × 3 rows).");
        assert_eq!(table_of(&ws, id).rows.len(), 3);
        assert_eq!(table_of(&ws, id).rows[2], ["Ink", "9", ""]);
    }

    #[test]
    fn changing_checks_what_it_is_given() {
        let mut ws = data::seed();
        let id = write(&mut ws, CREATE_TABLE, &json!({"channel": "general", "title": "T",
            "columns": ["A", "B"], "rows": [["1", "2"]]})).unwrap().id;
        let before = table_of(&ws, id).clone();
        let cases = [
            (UPDATE_TABLE, json!({"rows": []}), "`id` is required"),
            (UPDATE_TABLE, json!({"id": "T", "rows": []}), "`id` must be a number"),
            (UPDATE_TABLE, json!({"id": 99, "rows": []}), "There is no artifact 99. list_artifacts"),
            (UPDATE_TABLE, json!({"id": id}), "Nothing to change"),
            (UPDATE_TABLE, json!({"id": id, "title": ""}), "`title` is blank"),
            (UPDATE_TABLE, json!({"id": id, "columns": ["X"]}), "Give `rows` with `columns`"),
            (UPDATE_TABLE, json!({"id": id, "rows": [["1"]]}), "Row 1 has 1 cells, but the table has 2 columns (A, B)"),
            (UPDATE_TABLE, json!({"id": id, "columns": ["X"], "rows": [["1", "2"]]}), "Row 1 has 2 cells, but the table has 1 columns (X)"),
            (ADD_TABLE_ROWS, json!({"id": id, "rows": []}), "give at least one row"),
            (ADD_TABLE_ROWS, json!({"id": id}), "`rows` must be an array"),
            (ADD_TABLE_ROWS, json!({"id": id, "rows": [["1", "2", "3"]]}), "Row 1 has 3 cells"),
            (ADD_TABLE_ROWS, json!({"id": id, "rows": vec![json!(["", ""]); MAX_ROWS]}), "That makes 1001 rows"),
            (READ_ARTIFACT, json!({"id": 99}), "There is no artifact 99"),
            (LIST_ARTIFACTS, json!({}), "`channel` is required"),
        ];
        for (name, input, want) in cases {
            let e = match is_write(name) {
                true => err(write(&mut ws, name, &input)),
                false => err(read(&ws, name, &input)),
            };
            assert!(e.contains(want), "{name} {input}: {e}");
        }
        assert_eq!(*table_of(&ws, id), before, "a call that failed changed nothing");
        assert_eq!(ws.artifact(id).unwrap().title, "T");

        // A table whose file couldn't be read is left alone.
        ws.artifact_mut(id).unwrap().content = None;
        ws.artifact_mut(id).unwrap().load_error = Some("1.json: bad".into());
        for (name, input) in [(UPDATE_TABLE, json!({"id": id, "title": "U"})), (ADD_TABLE_ROWS, json!({"id": id, "rows": [["1", "2"]]}))] {
            let e = err(write(&mut ws, name, &input));
            assert_eq!(e, format!("Artifact {id} “T” can't be used: its file couldn't be read (1.json: bad). Only the owner can fix or delete it."));
        }
        assert!(err(read(&ws, READ_ARTIFACT, &json!({"id": id}))).contains("couldn't be read"));
        assert!(read(&ws, LIST_ARTIFACTS, &json!({"channel": "general"})).unwrap().contains("(its file can't be read)"));
    }

    #[test]
    fn renaming_keeps_titles_apart_in_a_channel() {
        let mut ws = data::seed();
        let make = |ws: &mut Workspace, channel: &str, title: &str| {
            write(ws, CREATE_TABLE, &json!({"channel": channel, "title": title, "columns": ["A"]})).unwrap().id
        };
        let (a, b) = (make(&mut ws, "general", "Plan"), make(&mut ws, "general", "Budget"));
        make(&mut ws, "engineering", "Ideas");
        let e = err(write(&mut ws, UPDATE_TABLE, &json!({"id": b, "title": "plan", "rows": [["x"]]})));
        assert_eq!(e, format!("#general already has a table titled “Plan” (id {a}). Change it with update_table, or pick another title."));
        assert_eq!((ws.artifact(b).unwrap().title.as_str(), table_of(&ws, b).rows.len()), ("Budget", 0), "nothing changed");
        // Its own title in other case, and a title used in another channel, are fine.
        assert!(write(&mut ws, UPDATE_TABLE, &json!({"id": a, "title": "PLAN"})).unwrap().title);
        assert!(write(&mut ws, UPDATE_TABLE, &json!({"id": b, "title": "Ideas"})).unwrap().title);
    }

    #[test]
    fn every_agent_gets_the_tools_and_the_ui_says_what_they_do() {
        let names: Vec<&str> = specs().iter().map(|t| t.name).collect();
        assert_eq!(names, ALL);
        // Agents start with the reading tools; each kind's writing tools come when asked for.
        let names: Vec<&str> = tools::specs().iter().map(|t| t.name).collect();
        assert!(names.ends_with(&[LIST_ARTIFACTS, READ_ARTIFACT, USE_ARTIFACT_TOOLS]), "{names:?}");
        assert!(!names.iter().any(|t| is_write(t)), "{names:?}");
        assert!(ALL.iter().all(|t| !is_write(t) || kind_of(t).is_some()), "every writing tool has a kind");
        let mut all = tools::specs();
        load_all(&mut all);
        let names: Vec<&str> = all.iter().map(|t| t.name).collect();
        assert!(ALL.iter().all(|t| names.contains(t)) && !names.contains(&USE_ARTIFACT_TOOLS), "{names:?}");
        assert_eq!(tools::activity(USE_ARTIFACT_TOOLS, &json!({"kinds": ["board"]})), "Getting board tools…");
        assert_eq!(tools::activity(CREATE_TABLE, &json!({"channel": "#general", "title": "Budget"})), "Creating table “Budget” in #general…");
        assert_eq!(tools::activity(UPDATE_TABLE, &json!({"id": 3})), "Updating table 3…");
        assert_eq!(tools::activity(ADD_TABLE_ROWS, &json!({"id": 3})), "Adding rows to table 3…");
        assert_eq!(tools::activity(READ_ARTIFACT, &json!({"id": 3})), "Reading artifact 3…");
        assert_eq!(tools::activity(LIST_ARTIFACTS, &json!({"channel": "general"})), "Looking at #general's artifacts…");
        // Writing tools are the app's to run: tools::run doesn't change anything.
        let ws = data::seed();
        assert!(tools::run(&ws, CREATE_TABLE, &json!({})).1);
    }
}
