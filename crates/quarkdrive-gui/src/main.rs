use quarkdrive_gui::app::App;
use quarkdrive_gui::ui;

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 720.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Quarkdrive",
        options,
        Box::new(|_cc| Box::new(Wrapper(App::new())) as Box<dyn eframe::App>),
    )
}

/// eframe wants an `eframe::App`; all real behaviour lives in [`App`].
struct Wrapper(App);

impl eframe::App for Wrapper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ui::draw(&mut self.0, ctx);
    }
}
