//! herdr as a sluice session, for `sluice switch herdr:<session> [-c DIR]`.
//!
//! [herdr](https://herdr.dev) is an agent multiplexer with sessions of its own.
//! herdr session X runs in one sluice session of its own, `herdr` (herdr's
//! default session) or `herdr-<name>`, so it never runs inside your other
//! sessions and herdr and sluice share the one terminal window. The switch
//! creates that session when it is missing, running herdr with sluice's handoff
//! on (`handoff`), so sluice passes every key to herdr; when herdr exits the
//! handoff ends with it, and so does the session. While herdr runs there it is
//! an ordinary session in the picker; once you detach from herdr it is gone.

use crate::picker::Choice;

/// How `sluice switch` names a herdr session: `herdr:<session>`. `:` never
/// survives `zmx::sanitize_name`, so no real session can be spelled this way.
const TAG: &str = "herdr:";

/// What the zmx session started for herdr runs. The handoff takes this shell's
/// pid, which `exec` hands to herdr, so a dead herdr ends it. ZMX_SESSION goes
/// so a herdr server started from here does not think it lives in a session.
/// Arguments: `$0` a label, `$1` sluice, the rest the herdr command line.
const LAUNCH: &str =
    r#"s=$1; shift; "$s" handoff on --pid $$ >/dev/null 2>&1; exec env -u ZMX_SESSION -u SLUICE "$@""#;

/// True for `herdr:<session>`, the name `sluice switch` takes for herdr.
pub fn is_tag(name: &str) -> bool {
    name.strip_prefix(TAG).is_some_and(|s| !s.is_empty())
}

/// The zmx session a herdr session runs in when started from sluice.
pub fn session_name(herdr: &str) -> String {
    match herdr {
        "default" => "herdr".to_string(),
        other => crate::zmx::sanitize_name(&format!("herdr-{other}")),
    }
}

/// The session to switch to: `herdr:<s>` becomes herdr session s's zmx
/// session, created in `dir` (home when unknown) running `command`
/// (`herdr-command`, plain `herdr` when unset). Anything else is a session name.
pub fn choice(picked: &str, command: Option<&str>, dir: Option<String>) -> Choice {
    let Some(herdr) = picked.strip_prefix(TAG) else {
        return Choice::named(picked);
    };
    Choice {
        name: session_name(herdr),
        cwd: dir.filter(|d| !d.is_empty()).or_else(|| std::env::var("HOME").ok()),
        cmd: Some(launch(herdr, &sluice_exe(), command.unwrap_or("herdr"))),
    }
}

/// `sh -c LAUNCH sluice-herdr <sluice> <command words> [--session X]`. A
/// leading `~/` in the command is expanded here: no shell sees it unquoted.
fn launch(herdr: &str, sluice: &str, command: &str) -> Vec<String> {
    let mut argv: Vec<String> = ["sh", "-c", LAUNCH, "sluice-herdr", sluice]
        .iter()
        .map(|s| s.to_string())
        .collect();
    argv.extend(command.split_whitespace().map(expand_home));
    if herdr != "default" {
        argv.extend(["--session".to_string(), herdr.to_string()]);
    }
    argv
}

fn expand_home(word: &str) -> String {
    match word.strip_prefix("~/") {
        Some(rest) => crate::clients::home().join(rest).display().to_string(),
        None => word.to_string(),
    }
}

fn sluice_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "sluice".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn tags_name_herdr_sessions_only() {
        assert!(is_tag("herdr:default"));
        assert!(is_tag("herdr:work"));
        assert!(!is_tag("herdr:"));
        assert!(!is_tag("herdr"));
        assert!(!is_tag("glass"));
    }

    #[test]
    fn session_names() {
        assert_eq!(session_name("default"), "herdr");
        assert_eq!(session_name("work"), "herdr-work");
        assert_eq!(session_name("a.b"), "herdr-a_b");
    }

    #[test]
    fn hidden_field_is_never_a_session_name() {
        assert_ne!(crate::zmx::sanitize_name("herdr:default"), "herdr:default");
    }

    #[test]
    fn choice_for_a_herdr_tag_launches_herdr_with_the_handoff() {
        let c = choice("herdr:work", None, Some("/code/glass".into()));
        assert_eq!(c.name, "herdr-work");
        assert_eq!(c.cwd.as_deref(), Some("/code/glass"), "starts where you picked it");
        let cmd = c.cmd.expect("a command to create the session with");
        assert_eq!(&cmd[..3], &["sh", "-c", LAUNCH]);
        assert_eq!(cmd[5..], ["herdr", "--session", "work"]);
        let d = choice("herdr:default", None, None).cmd.unwrap();
        assert_eq!(d.last().map(String::as_str), Some("herdr"));
    }

    #[test]
    fn choice_uses_the_configured_command_with_home_expanded() {
        let c = choice("herdr:default", Some("~/bin/herdr-at --quiet"), None);
        let cmd = c.cmd.unwrap();
        let home = crate::clients::home().display().to_string();
        assert_eq!(cmd[5..], [format!("{home}/bin/herdr-at"), "--quiet".to_string()]);
        let named = choice("herdr:ops", Some("/x/herdr-at"), None).cmd.unwrap();
        assert_eq!(named[5..], ["/x/herdr-at", "--session", "ops"]);
    }

    #[test]
    fn choice_for_anything_else_is_a_plain_session() {
        let c = choice("main", Some("/x/herdr-at"), Some("/d".into()));
        assert_eq!(c.name, "main");
        assert!(c.cmd.is_none());
    }

    #[test]
    fn launch_script_hands_off_the_pid_herdr_then_runs_as() {
        // A stand-in for sluice records the handoff request; a stand-in for
        // herdr prints its own pid. exec must keep the shell's pid, which is
        // the one handed off, or the handoff would end at once.
        let dir = std::env::temp_dir().join(format!("sluice-herdr-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("fake-sluice");
        let log = dir.join("handoff");
        std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" > '{}'\n", log.display())).unwrap();
        Command::new("chmod").args(["+x"]).arg(&fake).status().unwrap();
        let out = Command::new("sh")
            .args(["-c", LAUNCH, "sluice-herdr"])
            .arg(&fake)
            .args(["sh", "-c", "echo $$; echo ZMX=${ZMX_SESSION:-unset}"])
            .env("ZMX_SESSION", "outer")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let handoff = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        let mut lines = text.lines();
        let herdr_pid = lines.next().unwrap_or("").trim().to_string();
        assert_eq!(handoff.trim(), format!("handoff on --pid {herdr_pid}"), "{text}");
        assert_eq!(lines.next(), Some("ZMX=unset"), "herdr must not inherit ZMX_SESSION");
    }
}
