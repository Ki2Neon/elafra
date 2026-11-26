fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "windows")]
    {
        let resource = "assets/elafra.rc";
        println!("cargo:rerun-if-changed={resource}");
        println!("cargo:rerun-if-changed=assets/elafra.ico");
        embed_resource::compile(resource, embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed the Elafra icon");
    }
}
