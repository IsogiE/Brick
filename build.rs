fn main() {
    for name in [
        "BRICK_ADDON_PUBLIC_KEY_B64",
        "BRICK_DISCORD_CLIENT_ID",
        "BRICK_DISCORD_GUILD_ID",
        "BRICK_DISCORD_ALLOWED_ROLE_IDS",
        "BRICK_DISCORD_GUILD_NAME",
        "BRICK_DISCORD_ALLOWED_ROLE_LABEL",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("src/assets/brick.ico");
        resource.set("ProductName", "Brick");
        resource.set("FileDescription", "Brick");
        resource.set("CompanyName", "Advance");
        resource.set("LegalCopyright", "Advance");
        resource
            .compile()
            .expect("failed to compile Windows resources");
    }
}
