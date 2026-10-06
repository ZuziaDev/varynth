use std::fs;

use tempfile::tempdir;
use varynth::agent_files::{AgentFiles, AGENT_FILE_NAMES, MAX_SYSTEM_CONTEXT_BYTES};

#[test]
fn load_reads_global_and_project_layers() {
    let root = tempdir().unwrap();
    let project = root.path().join(".varynth");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("SOUL.md"), "project soul").unwrap();
    fs::write(project.join("MEMORY.md"), "project memory").unwrap();

    let files = AgentFiles::load(root.path()).unwrap();
    let context = files.system_context();
    assert!(context.contains("name=\"SOUL.md\" scope=\"project\""));
    assert!(context.contains("project soul"));
    assert!(context.contains("project memory"));
    assert!(context.contains("</varynth-agent-file>"));
}

#[test]
fn ensure_creates_missing_files_without_overwriting() {
    let root = tempdir().unwrap();
    let project = root.path().join(".varynth");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("SOUL.md"), "keep this content").unwrap();

    AgentFiles::ensure(root.path()).unwrap();
    assert_eq!(
        fs::read_to_string(project.join("SOUL.md")).unwrap(),
        "keep this content"
    );
    for name in AGENT_FILE_NAMES {
        assert!(project.join(name).exists() || project.join(name.to_ascii_lowercase()).exists());
    }
}

#[test]
fn append_memory_is_project_scoped_and_bounded() {
    let root = tempdir().unwrap();
    let mut files = AgentFiles::load(root.path()).unwrap();
    files.append_memory("Remember this decision.").unwrap();
    let memory = fs::read_to_string(root.path().join(".varynth/MEMORY.md")).unwrap();
    assert!(memory.contains("Remember this decision."));
    assert!(files.system_context().contains("Remember this decision."));
}

#[test]
fn context_has_a_hard_upper_bound() {
    let root = tempdir().unwrap();
    let project = root.path().join(".varynth");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("SOUL.md"),
        "x".repeat(MAX_SYSTEM_CONTEXT_BYTES * 2),
    )
    .unwrap();

    let context = AgentFiles::load(root.path()).unwrap().system_context();
    assert!(context.len() <= MAX_SYSTEM_CONTEXT_BYTES);
    assert!(context.contains("[truncated by Varynth]"));
}
