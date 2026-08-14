use std::io::{self, Read};

use anyhow::{Context, Result};
use reginux_core::privileged::{
    handle_privileged_request, PrivilegedRequest, PrivilegedResponse, HELPER_MESSAGE_LIMIT,
};

/// Minimal privileged boundary. It has no command execution or arbitrary path
/// API; all authorization and compare-and-replace checks live in the core.
fn main() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("reginux-helper must be launched as root through polkit");
    }
    let mut input = Vec::new();
    io::stdin()
        .take(HELPER_MESSAGE_LIMIT + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > HELPER_MESSAGE_LIMIT {
        anyhow::bail!("helper request exceeds the 8 MiB limit");
    }
    let request: PrivilegedRequest =
        serde_json::from_slice(&input).context("parse helper request")?;
    let result = handle_privileged_request(request);
    let response = match result {
        Ok(response) => response,
        Err(error) => PrivilegedResponse {
            ok: false,
            message: error.to_string(),
            backups: Vec::new(),
        },
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}
