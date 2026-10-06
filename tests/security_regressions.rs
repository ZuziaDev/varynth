use varynth::sandbox::{Jail, SandboxMode};
use varynth::session::Session;

#[test]
fn shell_chaining_is_rejected() {
    let jail = Jail::new(
        std::env::current_dir().unwrap(),
        vec![],
        SandboxMode::WorkspaceWrite,
        vec!["git".into()],
    );
    assert!(jail.assert_shell("git; whoami").is_err());
    assert!(jail.assert_shell("git && whoami").is_err());
    assert!(jail.assert_shell("git $(whoami)").is_err());
}

#[test]
fn session_ids_cannot_escape_the_sessions_directory() {
    assert!(Session::load("..\\config").is_err());
    assert!(Session::load("not-a-uuid").is_err());
}
