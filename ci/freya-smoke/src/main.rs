use freya::prelude::*;

fn app() -> impl IntoElement {
    let mut nested = use_state(|| false);
    let mut adjacent = use_state(|| false);
    let mut down = use_state(|| false);
    let password = use_state(String::new);
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .spacing(12.)
        .child(
            Input::new(password).placeholder("Password").trailing(
                Button::new()
                    .on_pointer_down(move |_| down.set(true))
                    .on_press(move |_| nested.set(true))
                    .child("Nested"),
            ),
        )
        .child(
            Button::new()
                .on_pointer_down(move |_| down.set(true))
                .on_press(move |_| adjacent.set(true))
                .child("Adjacent"),
        )
        .child(if down() {
            "pointer down"
        } else {
            "no pointer down"
        })
        .child(if nested() {
            "nested clicked"
        } else {
            "nested idle"
        })
        .child(if adjacent() {
            "adjacent clicked"
        } else {
            "adjacent idle"
        })
}

fn main() {
    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new(app)
                .with_title("Freya 0.5.2 Waypuppet test")
                .with_size(480., 240.),
        ),
    );
}
