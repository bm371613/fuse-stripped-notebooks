use std::io;

use serde_json::Value;

pub fn strip_outputs(raw: &[u8]) -> Result<Vec<u8>, io::Error> {
    let mut notebook: Value = serde_json::from_slice(raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(cells) = notebook["cells"].as_array_mut() {
        for cell in cells {
            if cell["cell_type"] == "code" {
                cell["outputs"] = Value::Array(vec![]);
                cell["execution_count"] = Value::Null;
            }
        }
    }
    serde_json::to_vec(&notebook).map_err(io::Error::other)
}

pub fn to_python_script(raw: &[u8]) -> Result<Vec<u8>, io::Error> {
    let notebook: Value = serde_json::from_slice(raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let cells = match notebook["cells"].as_array() {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let cell_type = cell["cell_type"].as_str().unwrap_or("");
        let source = &cell["source"];
        let lines: Vec<&str> = match source {
            Value::String(s) => s.split_inclusive('\n').collect(),
            Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
            _ => continue,
        };
        let header = format!("# --- cell {} ---\n", i + 1);
        out.extend_from_slice(header.as_bytes());
        match cell_type {
            "markdown" | "raw" => {
                out.extend_from_slice(b"\"\"\"\n");
                for line in &lines {
                    out.extend_from_slice(line.trim_end_matches('\n').as_bytes());
                    out.push(b'\n');
                }
                out.extend_from_slice(b"\"\"\"\n");
            }
            "code" => {
                for line in &lines {
                    out.extend_from_slice(line.trim_end_matches('\n').as_bytes());
                    out.push(b'\n');
                }
            }
            _ => {}
        }
    }
    Ok(out)
}
