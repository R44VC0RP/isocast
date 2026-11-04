//! The daemon's local fallback dialog, also used by CLI screen_pick. Only
//! the list index comes back; the daemon owns all X11 resources and ids.
use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc};

slint::slint! {
    import { Theme } from "../ui/theme.slint";
    import { Button, ButtonVariant, Dropdown, DropdownOption } from "../ui/kit.slint";
    export component X11Picker inherits Window {
        in property <string> heading;
        in property <[DropdownOption]> options;
        callback finish(int);
        title: "Isocast — " + heading;
        background: Theme.surface-overlay;
        width: 424px; height: 152px;
        VerticalLayout {
            padding: 16px; spacing: 10px;
            Text { text: root.heading; color: Theme.text; font-size: 16px; }
            choice := Dropdown { options: root.options; label: root.heading; value: "0"; }
            HorizontalLayout {
                alignment: end; spacing: 8px;
                Button { text: "Cancel"; clicked => { root.finish(-1); } }
                Button { text: "Share"; variant: ButtonVariant.primary; clicked => { root.finish(round(choice.value.to-float())); } }
            }
        }
    }
}

pub fn run() -> Result<()> {
    let (heading, labels): (String, Vec<String>) =
        serde_json::from_reader(std::io::stdin().lock())?;
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia-software".into())
        .select()?;
    let picker = X11Picker::new()?;
    picker.set_heading(heading.into());
    picker.set_options(ModelRc::from(
        labels
            .into_iter()
            .enumerate()
            .map(|(i, label)| DropdownOption {
                label: label.into(),
                value: i.to_string().into(),
                ..Default::default()
            })
            .collect::<Vec<_>>()
            .as_slice(),
    ));
    let result = std::rc::Rc::new(std::cell::Cell::new(-1));
    let answer = result.clone();
    picker.on_finish(move |index| {
        answer.set(index);
        let _ = slint::quit_event_loop();
    });
    picker.run().context("showing X11 choices")?;
    println!("{}", result.get());
    Ok(())
}
