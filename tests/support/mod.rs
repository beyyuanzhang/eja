pub fn control() -> eja::Control {
    eja::Control::new(std::time::Duration::from_secs(300), Default::default())
}
