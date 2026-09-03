fn main() {
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
