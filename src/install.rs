use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub fn run(startup: bool) -> Result<()> {
    let exe = env::current_exe().context("current_exe")?;
    let dest_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin");
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join("varynth.exe");
    fs::copy(&exe, &dest)
        .with_context(|| format!("copy {} → {}", exe.display(), dest.display()))?;
    println!("installed {}", dest.display());

    if startup {
        let xml = startup_xml(&dest);
        let tmp = env::temp_dir().join("varynth-serve.xml");
        fs::write(&tmp, xml)?;
        let status = Command::new("schtasks")
            .args([
                "/Create",
                "/TN",
                "VarynthServe",
                "/XML",
                tmp.to_str().unwrap_or(""),
                "/F",
            ])
            .status();
        match status {
            Ok(s) if s.success() => {
                println!("logon task VarynthServe registered (`varynth serve`)")
            }
            Ok(s) => anyhow::bail!("schtasks failed: {s}"),
            Err(e) => anyhow::bail!("schtasks: {e}"),
        }
    } else {
        println!("optional: varynth install --startup   # serve on Windows logon");
    }
    Ok(())
}

fn startup_xml(exe: &std::path::Path) -> String {
    let path = exe.display().to_string().replace('&', "&amp;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{path}</Command>
      <Arguments>serve</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}
