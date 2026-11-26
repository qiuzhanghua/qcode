//! `qc config show` 与 `qc doctor` 不得把 OAuth2 的 client secret 打出来。
//!
//! 为什么是端到端而不是单测：这两处输出是**手写的 println 分支**，没有共同的
//! 「安全格式化」函数可以单测。真正要断言的是「真实二进制面对一份真实配置时，
//! 屏幕上出现的字里没有那个值」——而这正是单测覆盖不了的那一层。
//!
//! 同 [`log_redaction`](./log_redaction.rs) 的理由：漏一个分支不会让任何别的测试
//! 变红，只会让密钥出现在一个用户会截图、会贴给别人看的地方。
//!
//! 两件事都要测，缺一不可：
//!
//! 1. **不泄漏**：输出里没有 secret 的值。
//! 2. **不沉默**：输出里确实有 oauth2 那一行（否则「没有值」是因为根本没打印，
//!    这条测试就变成了空断言）。

use std::path::Path;
use std::process::{Command, Output};

/// 足够长、足够独特：任何「碰巧被截断成前缀」都不会造成假通过。
const SECRET: &str = "oauth2-client-secret-MUST-NOT-BE-PRINTED-7d3a";

fn qc(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qc"))
        // 钉住语言（ADR-0105 §6）：这两条命令的散文跟随界面语言。
        .env("QCODE_LANG", "en")
        .arg("--config")
        .arg(config)
        .args(args)
        // 任何继承来的 key 都会被一起装进 redact 集合；这里只关心 agent 自己
        // 打出来的字，所以清掉，让输出只取决于这份 fixture。
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run qc")
}

fn fixture(secret_source: &str) -> String {
    format!(
        r#"
[providers.oauth2]
base_url = "https://gw.example.com/v1"
default_model = "llm-v1"

[providers.oauth2.oauth2]
mode = "authorization_code"
client_id = "qcode"
{secret_source}
auth_url = "https://auth.example.com/oauth2/auth"
token_url = "https://auth.example.com/oauth2/token"

[default-model]
provider = "oauth2"
model = "llm-v1"
"#
    )
}

fn temp_config(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, contents).expect("write config");
    (dir, path)
}

/// 明文来源：`config show` 只报「plainttext」这个类别，不报值。
#[test]
fn config_show_reports_the_source_and_never_the_secret() {
    let (_dir, config) = temp_config(&fixture(&format!("client_secret = \"{SECRET}\"")));
    let output = qc(&config, &["config", "show"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains(SECRET),
        "the OAuth2 client secret was printed by `qc config show`:\n{stdout}"
    );
    // 不沉默：那一行确实在，否则上面那条断言是空过的。
    assert!(
        stdout.contains("oauth2:"),
        "no oauth2 line was printed, so the leak check proves nothing:\n{stdout}"
    );
    assert!(
        stdout.contains("client_secret=<plaintext>"),
        "the source was not reported as plaintext:\n{stdout}"
    );
}

/// 环境变量来源：报**变量名**（那是用户自己填的、本来就可见的），仍不报值。
#[test]
fn config_show_reports_the_variable_name_not_its_value() {
    let (_dir, config) = temp_config(&fixture("client_secret_env = \"CORP_OAUTH_SECRET\""));
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .env("QCODE_LANG", "en")
        .env("CORP_OAUTH_SECRET", SECRET)
        .arg("--config")
        .arg(&config)
        .args(["config", "show"])
        .output()
        .expect("run qc");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains(SECRET),
        "the resolved secret was printed by `qc config show`:\n{stdout}"
    );
    assert!(
        stdout.contains("CORP_OAUTH_SECRET"),
        "the variable name should be visible (it is not the secret):\n{stdout}"
    );
}

/// 明文来源时 doctor 提示一次「建议改用 env / command」——但同样不报值。
#[test]
fn doctor_warns_about_a_plaintext_secret_without_printing_it() {
    let (_dir, config) = temp_config(&fixture(&format!("client_secret = \"{SECRET}\"")));
    let output = qc(&config, &["doctor"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains(SECRET),
        "the OAuth2 client secret was printed by `qc doctor`:\n{stdout}"
    );
    assert!(
        stdout.contains("OAuth2 client secret"),
        "doctor did not mention the OAuth2 secret at all:\n{stdout}"
    );
    assert!(
        stdout.contains("client_secret_env") || stdout.contains("client_secret_command"),
        "the warning should name the alternatives:\n{stdout}"
    );
}

/// 非明文来源时不该多嘴：一条「你已经做对了」的 warn 会把真正的告警淹掉。
#[test]
fn doctor_stays_quiet_when_the_secret_comes_from_the_environment() {
    let (_dir, config) = temp_config(&fixture("client_secret_env = \"CORP_OAUTH_SECRET\""));
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .env("QCODE_LANG", "en")
        .env("CORP_OAUTH_SECRET", SECRET)
        .arg("--config")
        .arg(&config)
        .args(["doctor"])
        .output()
        .expect("run qc");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains(SECRET),
        "the resolved secret was printed by `qc doctor`:\n{stdout}"
    );
    assert!(
        !stdout.contains("OAuth2 client secret"),
        "doctor warned about a secret that is not in the file:\n{stdout}"
    );
}
