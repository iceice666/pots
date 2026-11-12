//! The tools agents use on a channel's artifacts: list them and read any
//! kind; create and change tables. Writing tools are per kind
//! (`create_table`, `update_table`, …), since each kind's content is shaped
//! differently; documents and whiteboards will bring their own. Deleting
//! is left to people.
//!
//! The functions here only change the [`Workspace`]; the app runs the
//! writing ones (see [`is_write`]) itself, to save the artifact's file and
//! show the change in an editor that has it open.

use serde_json::{Value, json};

use crate::agent::tools::channel;
use crate::data::{ArtifactContent, Table, Workspace};
use pots_llm::ToolSpec;

pub const LIST_ARTIFACTS: &str = "list_artifacts";
pub const READ_ARTIFACT: &str = "read_artifact";
pub const CREATE_TABLE: &str = "create_table";
pub const UPDATE_TABLE: &str = "update_table";
pub const ADD_TABLE_ROWS: &str = "add_table_rows";

/// Every artifact tool, in the order agents are offered them.
pub const ALL: [&str; 5] = [LIST_ARTIFACTS, READ_ARTIFACT, CREATE_TABLE, UPDATE_TABLE, ADD_TABLE_ROWS];

/// The most a table made or changed by an agent may have.
pub const MAX_COLUMNS: usize = 50;
pub const MAX_ROWS: usize = 1000;
/// The longest artifact title, in characters.
pub const MAX_TITLE: usize = 100;

pub fn is_tool(name: &str) -> bool {
    ALL.contains(&name)
}

/// Whether tool `name` changes an artifact (the rest only read).
pub fn is_write(name: &str) -> bool {
    matches!(name, CREATE_TABLE | UPDATE_TABLE | ADD_TABLE_ROWS)
}

const ROWS_DESCRIPTION: &str = "The rows, top to bottom: each an array of cell texts, one per column in column \
                                order. Use \"\" for an empty cell.";

pub fn specs() -> Vec<ToolSpec> {
    let rows = json!({"type": "array", "items": {"type": "array", "items": {"type": "string"}}, "description": ROWS_DESCRIPTION});
    let columns = json!({"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Column names, left to right."});
    let id = json!({"type": "integer", "minimum": 0, "description": "The artifact's id, from list_artifacts."});
    vec![
        ToolSpec {
            name: LIST_ARTIFACTS,
            description: "List a channel's artifacts (tables people keep there, shown as tabs above its messages): \
                          each one's id, kind and title.",
            input_schema: json!({
                "type": "object",
                "properties": {"channel": {"type": "string", "description": "Channel name, without the #."}},
                "required": ["channel"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: READ_ARTIFACT,
            description: "Read an artifact as it is now, including the owner's latest edits. For a table: its \
                          title, columns and rows, as JSON. Read a table again before changing it: people may \
                          have edited it since.",
            input_schema: json!({
                "type": "object",
                "properties": {"id": id},
                "required": ["id"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: CREATE_TABLE,
            description: "Create a table artifact in a channel: a new tab there that people can open and edit \
                          like a spreadsheet. Cells are plain text. Every row needs exactly one cell per column. \
                          Returns the new table's id.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "channel": {"type": "string", "description": "Channel name, without the #."},
                    "title": {"type": "string", "description": "A few words saying what the table holds."},
                    "columns": columns,
                    "rows": rows
                },
                "required": ["channel", "title", "columns"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: UPDATE_TABLE,
            description: "Change a table: its title, and/or replace all of its rows (and, with them, its \
                          columns). What you give replaces what's there, so read_artifact first and pass the \
                          rows you keep along with your changes. To only add rows at the bottom, use \
                          add_table_rows instead. Whatever the owner is typing in the table when you change its \
                          cells gives way to your version.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": id,
                    "title": {"type": "string", "description": "A new title."},
                    "columns": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                                "description": "New column names, left to right. Give `rows` too, shaped to them."},
                    "rows": rows
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: ADD_TABLE_ROWS,
            description: "Add rows at the bottom of a table, keeping what's there: the way to log entries in a \
                          table as they come.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": id,
                    "rows": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}, "minItems": 1,
                             "description": ROWS_DESCRIPTION}
                },
                "required": ["id", "rows"],
                "additionalProperties": false
            }),
        },
    ]
}

/// What the UI shows while an artifact tool runs.
pub fn activity(name: &str, input: &Value) -> Option<String> {
    let channel = input["channel"].as_str().map(|c| c.trim_start_matches('#'));
    let id = input["id"].as_u64();
    Some(match (name, channel, id) {
        (LIST_ARTIFACTS, Some(c), _) => format!("Looking at #{c}'s artifacts…"),
        (READ_ARTIFACT, _, Some(id)) => format!("Reading artifact {id}…"),
        (CREATE_TABLE, Some(c), _) => match input["title"].as_str() {
            Some(t) => format!("Creating table “{}” in #{c}…", t.trim()),
            None => format!("Creating a table in #{c}…"),
        },
        (UPDATE_TABLE, _, Some(id)) => format!("Updating table {id}…"),
        (ADD_TABLE_ROWS, _, Some(id)) => format!("Adding rows to table {id}…"),
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
        out.push_str(&format!("- id {id}: {} “{}”", a.kind.label().to_lowercase(), a.title));
        match &a.content {
            Some(ArtifactContent::Table(t)) => out.push_str(&format!(" ({})", size(t))),
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
    let place = &ws.channels[a.channel].name;
    match content {
        ArtifactContent::Table(t) => Ok(format!(
            "Table {id} “{}” in #{place}, {}:\n{}",
            a.title,
            size(t),
            json!({"columns": t.columns, "rows": t.rows})
        )),
    }
}

fn create_table(ws: &mut Workspace, input: &Value) -> Result<Changed, String> {
    let c = channel(ws, input)?;
    let title = title(&input["title"])?.ok_or("`title` is required.")?;
    if let Some(a) = ws.artifacts.iter().find(|a| a.channel == c && a.title.eq_ignore_ascii_case(&title)) {
        return Err(format!(
            "#{} already has a {} titled “{}” (id {}). Change it with update_table, or pick another title.",
            ws.channels[c].name,
            a.kind.label().to_lowercase(),
            a.title,
            a.id
        ));
    }
    let columns = columns(&input["columns"])?.ok_or("`columns` is required: the column names, left to right.")?;
    let rows = match &input["rows"] {
        Value::Null => Vec::new(),
        v => rows(v, &columns, 0)?,
    };
    let table = Table { columns, rows };
    let size = size(&table);
    let id = ws.add_artifact_with(c, title.clone(), ArtifactContent::Table(table));
    let place = &ws.channels[c].name;
    Ok(Changed {
        id,
        content: true,
        title: true,
        shown: format!("Created table “{title}” in #{place} ({size})."),
        result: format!(
            "Created table {id} “{title}” in #{place} ({size}). It is a tab in #{place} now; people can edit it \
             too, so read it again before changing it later."
        ),
    })
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
    let table = Table {
        columns: new_columns.unwrap_or_else(|| old.columns.clone()),
        rows: new_rows.unwrap_or_else(|| old.rows.clone()),
    };
    let content = table != *old;
    let size = size(&table);
    let a = ws.artifact_mut(id).expect("checked");
    let title = new_title.as_ref().is_some_and(|t| *t != a.title);
    if let Some(t) = new_title.filter(|_| title) {
        a.title = t;
    }
    if content {
        a.content = Some(ArtifactContent::Table(table));
    }
    let a = ws.artifact(id).expect("checked");
    let (name, place) = (a.title.clone(), ws.channels[a.channel].name.clone());
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
                result: format!("Table {id} “{name}” already says that; nothing changed."),
            });
        }
    };
    let what_cap = format!("{}{}", what[..1].to_uppercase(), &what[1..]);
    Ok(Changed {
        id,
        content,
        title,
        shown: format!("{what_cap} table “{name}” in #{place} ({size})."),
        result: format!("Table {id} “{name}” in #{place} is {what} ({size})."),
    })
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
    let size = size(&table);
    let a = ws.artifact_mut(id).expect("checked");
    a.content = Some(ArtifactContent::Table(table));
    let a = ws.artifact(id).expect("checked");
    let (name, place) = (a.title.clone(), ws.channels[a.channel].name.clone());
    let rows = if n == 1 { "1 row".to_string() } else { format!("{n} rows") };
    Ok(Changed {
        id,
        content: true,
        title: false,
        shown: format!("Added {rows} to table “{name}” in #{place} ({size})."),
        result: format!("Added {rows} to table {id} “{name}” in #{place}; it has {size} now."),
    })
}

// --- checking what the agent passed ----------------------------------------

/// "3 columns × 2 rows".
fn size(t: &Table) -> String {
    let plural = |n: usize, what: &str| if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") };
    format!("{} × {}", plural(t.columns.len(), "column"), plural(t.rows.len(), "row"))
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

/// Table `id`'s content, for the tools that change tables.
fn table(ws: &Workspace, id: usize) -> Result<&Table, String> {
    // Tables are the only kind yet. Another kind adds its arm here, an
    // error saying "Artifact {id} “…” is a document, not a table."
    match readable(ws, id)? {
        ArtifactContent::Table(t) => Ok(t),
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
        return Err("`title` is blank: give the table a few words saying what it holds.".into());
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
        }
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
    fn every_agent_gets_the_tools_and_the_ui_says_what_they_do() {
        let names: Vec<&str> = tools::specs().iter().map(|t| t.name).collect();
        assert!(ALL.iter().all(|t| names.contains(t)), "{names:?}");
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
