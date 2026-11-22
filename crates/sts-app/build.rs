fn main() {
    if let Err(err) = slint_build::compile("ui/app.slint") {
        panic!("failed to compile Slint UI (`ui/app.slint`): {err}");
    }
}
