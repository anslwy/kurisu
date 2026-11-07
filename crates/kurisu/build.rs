// Rebuild when the embedded web UI changes (include_dir! alone does not track it).
fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    for entry in walk("web/dist") {
        println!("cargo:rerun-if-changed={entry}");
    }
}

fn walk(dir: &str) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p.to_string_lossy()));
            } else {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}
