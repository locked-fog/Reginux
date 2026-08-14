use anyhow::Result;
use reginux_core::sandbox::{exec_request, read_request};

fn main() -> Result<()> {
    exec_request(read_request()?)
}
