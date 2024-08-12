use std::path::Path;
use std::process::Command;

fn main() {
    #[cfg(feature = "webui")]
    {
        let webui_dir = Path::new("webui");
        let webui_src_dir = webui_dir.join("src");

        println!("cargo:rerun-if-changed={}", webui_src_dir.to_str().unwrap());

        // Run "npm run build" in the webui directory
        let output = Command::new("npm")
            .arg("run")
            .arg("build")
            .current_dir(webui_dir)
            .output()
            .expect("Failed to execute npm run build");

        if !output.status.success() {
            panic!(
                "npm run build failed with output: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Optionally print the stdout output if you want to see the build logs
        println!("{}", String::from_utf8_lossy(&output.stdout));
    }
}
