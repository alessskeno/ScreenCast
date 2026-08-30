// assets/ffmpeg.exe varsa onu exe'ye gömme özelliğini aç.
// Dosya yoksa proje yine derlenir; motor sistemdeki ffmpeg'i arar.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(embedded_ffmpeg)");
    println!("cargo:rerun-if-changed=assets/ffmpeg.exe");
    if std::path::Path::new("assets/ffmpeg.exe").exists() {
        println!("cargo:rustc-cfg=embedded_ffmpeg");
    }
}
