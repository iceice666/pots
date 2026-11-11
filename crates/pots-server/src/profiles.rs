//! Agent profile（`proposal/agent-collaboration.md#profile-一覽`）。
//!
//! profile 是設定檔：內建的 `profiles/*.toml` 編進執行檔；資料目錄裡有同名的
//! `profiles/<name>.toml` 時用它取代（整份，不合併）。執行期認得 `chat`、`planner`、`worker` 與
//! 唯讀的 subagent（`explorer`、`reviewer`），只讀這幾個檔；工具清單與可寫的 artifact 種類依 agent
//! 檢查，不合法時服務不啟動。
//!
//! 程式碼的唯讀工具（`list_files`、`read_file`、`grep`、`git_log`，worker 另有 `git_diff`）不列在
//! profile 裡：run 看得到程式碼（worker 的工作目錄、subagent 指定的 repo）時自動都有。

use std::path::Path;

use pots_proto::ArtifactKind;
use serde::Deserialize;

use crate::Error;

const BUILTIN_CHAT: &str = include_str!("../profiles/chat.toml");
const BUILTIN_PLANNER: &str = include_str!("../profiles/planner.toml");
const BUILTIN_WORKER: &str = include_str!("../profiles/worker.toml");
const BUILTIN_EXPLORER: &str = include_str!("../profiles/explorer.toml");
const BUILTIN_REVIEWER: &str = include_str!("../profiles/reviewer.toml");
const BUILTIN_GLOBAL: &str = include_str!("../profiles/global.toml");

/// 唯讀基礎集以外、要在 profile 裡授權的工具。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    CreateGoal,
    SteerGoal,
    CancelGoal,
    /// 封存一個已結束的 Goal（從 project 列表與快照移除，資料仍留著）。
    ArchiveGoal,
    WriteArtifact,
    CreateTask,
    CancelTask,
    PostMilestone,
    CompleteGoal,
    /// 掛起任務等外部事件（worker 用；Planner 覆寫 profile 時可以讓自己 Goal 還沒開始的任務等待）。
    WaitFor,
    Schedule,
    CreateListener,
    /// 總管（M6）的工具；內建的 profile 還沒有。
    PauseAutomations,
    ResumeAutomations,
    /// 列出這個 project 的排程與 listener（對話 agent）。
    ListAutomations,
    /// 停用／啟用排程、改下一次與過期時間（對話 agent）。
    UpdateSchedule,
    /// 申請刪除排程（對話 agent；你核准後刪除）。
    DeleteSchedule,
    /// 停用／啟用 listener（對話 agent；等核准的提案不能用它啟用）。
    UpdateListener,
    /// 申請刪除 listener（對話 agent；你核准後刪除）。
    DeleteListener,
    /// 列出 artifact 的版本紀錄、讀某一版（對話 agent）。
    ArtifactHistory,
    /// 把 artifact 還原成某一版（新增一個版本；對話 agent）。
    RestoreArtifact,
    /// 申請封存 artifact（對話 agent；你核准後從列表移除，資料仍留著）。
    ArchiveArtifact,
    /// 快速通道：直接派一個 worker，不經 Planner。
    QuickFix,
    /// 開唯讀的 subagent（explorer、reviewer）。
    RunSubagent,
    PostToTeam,
    ReadTeam,
    /// 申請 merge 任務的 PR（worker、Planner；你核准後由平台 merge）。
    RequestApproval,
    /// 設定 project 的工作資料夾（對話 agent；使用者說「我們在哪個資料夾工作」時）。
    SetWorkdir,
    /// 提案修改 USER.md（對話 agent；你核准後寫入）。
    ProposeUserMdEdit,
    /// 提案修改 Project Notes（對話 agent、Planner；你核准後寫入）。
    ProposeProjectNote,
    /// 問你一個問題（可帶選項）；你回答後叫醒問的 agent（對話 agent、Planner、worker）。
    AskUser,
    // worker 的工具
    EditFile,
    Run,
    OpenPr,
    ReportProgress,
    CompleteTask,
    FailTask,
    Escalate,
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreateGoal => "create_goal",
            Self::SteerGoal => "steer_goal",
            Self::CancelGoal => "cancel_goal",
            Self::ArchiveGoal => "archive_goal",
            Self::WriteArtifact => "write_artifact",
            Self::CreateTask => "create_task",
            Self::CancelTask => "cancel_task",
            Self::PostMilestone => "post_milestone",
            Self::CompleteGoal => "complete_goal",
            Self::WaitFor => "wait_for",
            Self::Schedule => "schedule",
            Self::CreateListener => "create_listener",
            Self::PauseAutomations => "pause_automations",
            Self::ResumeAutomations => "resume_automations",
            Self::ListAutomations => "list_automations",
            Self::UpdateSchedule => "update_schedule",
            Self::DeleteSchedule => "delete_schedule",
            Self::UpdateListener => "update_listener",
            Self::DeleteListener => "delete_listener",
            Self::ArtifactHistory => "artifact_history",
            Self::RestoreArtifact => "restore_artifact",
            Self::ArchiveArtifact => "archive_artifact",
            Self::QuickFix => "quick_fix",
            Self::RunSubagent => "run_subagent",
            Self::PostToTeam => "post_to_team",
            Self::ReadTeam => "read_team",
            Self::RequestApproval => "request_approval",
            Self::SetWorkdir => "set_workdir",
            Self::ProposeUserMdEdit => "propose_user_md_edit",
            Self::ProposeProjectNote => "propose_project_note",
            Self::AskUser => "ask_user",
            Self::EditFile => "edit_file",
            Self::Run => "run",
            Self::OpenPr => "open_pr",
            Self::ReportProgress => "report_progress",
            Self::CompleteTask => "complete_task",
            Self::FailTask => "fail_task",
            Self::Escalate => "escalate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMode {
    /// 歷史由 Observational Memory 組成。
    Om,
    /// 每次 run 從狀態重建 context。
    Stateless,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub name: String,
    pub memory: MemoryMode,
    pub max_turns: usize,
    pub tools: Vec<ToolName>,
    #[serde(default)]
    pub artifact_kinds: Vec<ArtifactKind>,
    pub role_prompt: String,
}

impl AgentProfile {
    pub fn has(&self, tool: ToolName) -> bool {
        self.tools.contains(&tool)
    }

    pub fn role_prompt(&self) -> &str {
        self.role_prompt.trim()
    }
}

/// 執行期用到的 profile。
#[derive(Clone, Debug)]
pub struct Profiles {
    pub chat: AgentProfile,
    pub planner: AgentProfile,
    pub worker: AgentProfile,
    pub explorer: AgentProfile,
    pub reviewer: AgentProfile,
    /// 總管：跨 project 的全域對話（M6）。
    pub global: AgentProfile,
}

struct Rules {
    name: &'static str,
    memory: MemoryMode,
    tools: &'static [ToolName],
    kinds: &'static [ArtifactKind],
}

const CHAT_RULES: Rules = Rules {
    name: "chat",
    memory: MemoryMode::Om,
    tools: &[
        ToolName::CreateGoal,
        ToolName::SteerGoal,
        ToolName::CancelGoal,
        ToolName::ArchiveGoal,
        ToolName::WriteArtifact,
        ToolName::Schedule,
        ToolName::CreateListener,
        // 總管的工具；內建的 chat 沒有，覆寫時只能暫停自己的 project。
        ToolName::PauseAutomations,
        ToolName::ResumeAutomations,
        ToolName::QuickFix,
        ToolName::RunSubagent,
        ToolName::ProposeUserMdEdit,
        ToolName::ProposeProjectNote,
        // 使用者說「我們在哪個資料夾工作」時由對話 agent 設定。
        ToolName::SetWorkdir,
        // 管理這個 project 的自動化與 artifact；刪除與封存要你核准。
        ToolName::ListAutomations,
        ToolName::UpdateSchedule,
        ToolName::DeleteSchedule,
        ToolName::UpdateListener,
        ToolName::DeleteListener,
        ToolName::ArtifactHistory,
        ToolName::RestoreArtifact,
        ToolName::ArchiveArtifact,
        ToolName::AskUser,
    ],
    kinds: &[ArtifactKind::Spec, ArtifactKind::Report],
};

const PLANNER_RULES: Rules = Rules {
    name: "planner",
    memory: MemoryMode::Stateless,
    tools: &[
        ToolName::CreateTask,
        ToolName::CancelTask,
        ToolName::PostMilestone,
        ToolName::CompleteGoal,
        ToolName::WriteArtifact,
        ToolName::Schedule,
        // worker 的工具；內建的 planner 沒有，覆寫時可以讓自己 Goal 的任務等待。
        ToolName::WaitFor,
        ToolName::PostToTeam,
        ToolName::ReadTeam,
        ToolName::RunSubagent,
        ToolName::RequestApproval,
        ToolName::ProposeProjectNote,
        ToolName::AskUser,
    ],
    kinds: &[ArtifactKind::Ledger, ArtifactKind::Report],
};

const WORKER_RULES: Rules = Rules {
    name: "worker",
    memory: MemoryMode::Stateless,
    tools: &[
        ToolName::EditFile,
        ToolName::Run,
        ToolName::OpenPr,
        ToolName::WaitFor,
        ToolName::PostToTeam,
        ToolName::ReadTeam,
        ToolName::ReportProgress,
        ToolName::CompleteTask,
        ToolName::FailTask,
        ToolName::Escalate,
        ToolName::WriteArtifact,
        ToolName::RunSubagent,
        ToolName::RequestApproval,
        ToolName::AskUser,
    ],
    kinds: &[ArtifactKind::Report],
};

/// subagent 只有唯讀工具（不能再開 subagent）。
const EXPLORER_RULES: Rules = Rules {
    name: "explorer",
    memory: MemoryMode::Stateless,
    tools: &[],
    kinds: &[],
};

const REVIEWER_RULES: Rules = Rules {
    name: "reviewer",
    memory: MemoryMode::Stateless,
    tools: &[],
    kinds: &[],
};

/// 總管只看得到唯讀工具，加上跨 project 的派工、排程與暫停。
const GLOBAL_RULES: Rules = Rules {
    name: "global",
    memory: MemoryMode::Om,
    tools: &[
        ToolName::CreateGoal,
        ToolName::CancelGoal,
        ToolName::ArchiveGoal,
        ToolName::Schedule,
        ToolName::PauseAutomations,
        ToolName::ResumeAutomations,
        ToolName::RunSubagent,
    ],
    kinds: &[],
};

impl Profiles {
    /// 內建的 profile。
    #[cfg(test)]
    pub fn builtin() -> Self {
        Self {
            chat: parse(BUILTIN_CHAT, &CHAT_RULES, "內建 chat.toml").expect("內建 chat.toml"),
            planner: parse(BUILTIN_PLANNER, &PLANNER_RULES, "內建 planner.toml")
                .expect("內建 planner.toml"),
            worker: parse(BUILTIN_WORKER, &WORKER_RULES, "內建 worker.toml")
                .expect("內建 worker.toml"),
            explorer: parse(BUILTIN_EXPLORER, &EXPLORER_RULES, "內建 explorer.toml")
                .expect("內建 explorer.toml"),
            reviewer: parse(BUILTIN_REVIEWER, &REVIEWER_RULES, "內建 reviewer.toml")
                .expect("內建 reviewer.toml"),
            global: parse(BUILTIN_GLOBAL, &GLOBAL_RULES, "內建 global.toml")
                .expect("內建 global.toml"),
        }
    }

    /// 內建的 profile，資料目錄的 `profiles/<name>.toml` 存在時取代。
    pub fn load(data_dir: &Path) -> Result<Self, Error> {
        let dir = data_dir.join("profiles");
        let one = |builtin: &str, rules: &Rules| -> Result<AgentProfile, Error> {
            let path = dir.join(format!("{}.toml", rules.name));
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let p = parse(&text, rules, &path.display().to_string())?;
                    tracing::info!(path = %path.display(), "使用自訂的 profile");
                    Ok(p)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    parse(builtin, rules, &format!("內建 {}.toml", rules.name))
                }
                Err(e) => Err(Error::Io(e)),
            }
        };
        Ok(Self {
            chat: one(BUILTIN_CHAT, &CHAT_RULES)?,
            planner: one(BUILTIN_PLANNER, &PLANNER_RULES)?,
            worker: one(BUILTIN_WORKER, &WORKER_RULES)?,
            explorer: one(BUILTIN_EXPLORER, &EXPLORER_RULES)?,
            reviewer: one(BUILTIN_REVIEWER, &REVIEWER_RULES)?,
            global: one(BUILTIN_GLOBAL, &GLOBAL_RULES)?,
        })
    }
}

fn parse(text: &str, rules: &Rules, source: &str) -> Result<AgentProfile, Error> {
    let bad = |msg: String| Error::Config(format!("{source}：{msg}"));
    let p: AgentProfile = toml::from_str(text).map_err(|e| bad(e.to_string()))?;
    if p.name != rules.name {
        return Err(bad(format!("name 應該是 {}", rules.name)));
    }
    if p.memory != rules.memory {
        return Err(bad(format!("{} 的 memory 不能改", rules.name)));
    }
    if p.max_turns == 0 || p.max_turns > 100 {
        return Err(bad("max_turns 要在 1 到 100 之間".into()));
    }
    if let Some(t) = p.tools.iter().find(|t| !rules.tools.contains(t)) {
        return Err(bad(format!("{} 不能用 {}", rules.name, t.as_str())));
    }
    if let Some(k) = p.artifact_kinds.iter().find(|k| !rules.kinds.contains(k)) {
        return Err(bad(format!(
            "{} 不能用 write_artifact 寫 {}",
            rules.name,
            k.label()
        )));
    }
    if p.has(ToolName::WriteArtifact) && p.artifact_kinds.is_empty() {
        return Err(bad("有 write_artifact 時 artifact_kinds 不能是空的".into()));
    }
    if p.role_prompt.trim().is_empty() {
        return Err(bad("role_prompt 不能是空的".into()));
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_parse() {
        let p = Profiles::builtin();
        assert_eq!(p.chat.memory, MemoryMode::Om);
        assert!(p.chat.has(ToolName::CreateGoal));
        assert_eq!(
            p.chat.artifact_kinds,
            [ArtifactKind::Spec, ArtifactKind::Report]
        );
        for t in [
            ToolName::ListAutomations,
            ToolName::UpdateSchedule,
            ToolName::DeleteSchedule,
            ToolName::UpdateListener,
            ToolName::DeleteListener,
            ToolName::ArtifactHistory,
            ToolName::RestoreArtifact,
            ToolName::ArchiveArtifact,
        ] {
            assert!(p.chat.has(t), "{}", t.as_str());
        }
        assert!(p.planner.has(ToolName::CompleteGoal));
        assert!(p.planner.has(ToolName::RequestApproval));
        assert!(p.worker.has(ToolName::RequestApproval));
        assert!(p.chat.has(ToolName::ProposeUserMdEdit));
        assert!(p.chat.has(ToolName::AskUser));
        assert!(p.planner.has(ToolName::AskUser));
        assert!(p.worker.has(ToolName::AskUser));
        assert!(p.chat.has(ToolName::ProposeProjectNote));
        assert_eq!(p.planner.artifact_kinds, [ArtifactKind::Ledger]);
        assert!(!p.planner.role_prompt().ends_with('\n'));
    }

    #[test]
    fn data_dir_overrides_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        let path = tmp.path().join("profiles/chat.toml");
        std::fs::write(
            &path,
            "name = \"chat\"\nmemory = \"om\"\nmax_turns = 3\ntools = [\"create_goal\"]\nrole_prompt = \"自訂\"\n",
        )
        .unwrap();
        let p = Profiles::load(tmp.path()).unwrap();
        assert_eq!(p.chat.max_turns, 3);
        assert_eq!(p.chat.tools, [ToolName::CreateGoal]);
        assert_eq!(p.planner, Profiles::builtin().planner);

        std::fs::write(
            &path,
            "name = \"chat\"\nmemory = \"om\"\nmax_turns = 3\ntools = [\"create_task\"]\nrole_prompt = \"x\"\n",
        )
        .unwrap();
        let err = Profiles::load(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("chat 不能用 create_task"), "{err}");

        std::fs::write(&path, "name = \"chat\"\ntools = [\"fly\"]\n").unwrap();
        assert!(Profiles::load(tmp.path()).is_err());
    }
}
