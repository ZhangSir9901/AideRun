// AideRun - 编译构建脚本，在链接阶段自动嵌入 Windows 图标资源并监听素材变更
fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/logo.svg");
    println!("cargo:rerun-if-changed=assets/panel.html");
    println!("cargo:rerun-if-changed=assets/win_64.rgba");
    println!("cargo:rerun-if-changed=assets/tray_32.rgba");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let _ = embed_resource::compile("assets/app.rc", embed_resource::NONE);
    }
}

