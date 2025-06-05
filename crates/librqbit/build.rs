use anyhow::{Context, bail};
use std::path::Path;
use std::process::Command;

#[allow(dead_code)]
fn run_cmd(cwd: &Path, cmd: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    let (shell, shell_args) = ("powershell", ["-command"].as_slice());
    #[cfg(not(target_os = "windows"))]
    let (shell, shell_args) = ("sh", ["-c"].as_slice());

    // Run "npm install" in the webui directory
    let output = Command::new(shell)
        .args(shell_args)
        .arg(cmd)
        .current_dir(cwd)
        .output()
        .with_context(|| {
            format!(
                "Failed to execute {} in {:?}. PATH: {:?}",
                cmd,
                cwd,
                std::env::var("PATH").unwrap_or_default()
            )
        })?;

    if !output.status.success() {
        bail!(
            "\"{}\" failed\n\nstderr: {}\n\nstdout: {}",
            cmd,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    // Optionally print the stdout output if you want to see the build logs
    println!("{}", String::from_utf8_lossy(&output.stdout));

    Ok(())
}

fn main() {
    #[cfg(feature = "webui")]
    {
        let webui_dir = Path::new("webui");
        let webui_src_dir = webui_dir.join("src");

        println!("cargo:rerun-if-changed={}", webui_src_dir.to_str().unwrap());

        // Run "npm install && npm run build" in the webui directory
        for cmd in ["npm install", "npm run build"] {
            run_cmd(webui_dir, cmd).unwrap();
        }
    }
}
