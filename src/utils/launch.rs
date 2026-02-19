use std::process::Command;

pub fn open_dikt_ui(_preferred_page: Option<&str>) -> Result<(), String> {
    let launch_attempts: [(&str, &[&str]); 3] = [
        ("gtk-launch", &["io.dikt.Dikt"]),
        ("dikt", &[]),
        ("/usr/bin/dikt", &[]),
    ];

    let mut errors = Vec::new();
    for (program, args) in launch_attempts {
        match Command::new(program).args(args).spawn() {
            Ok(_) => return Ok(()),
            Err(e) => errors.push(format!("{} {:?}: {}", program, args, e)),
        }
    }

    Err(format!(
        "Unable to launch Dikt UI. Attempted: {}",
        errors.join(" | ")
    ))
}
