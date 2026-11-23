//! Slash commands: typing "/" in a bot chat lists what the bot offers.

use slint::{Model, ModelRc, VecModel};
use std::rc::Rc;

use crate::{CommandItem, with_app};

/// The commands matching what is typed so far, or the popup closing.
pub fn suggest(commands: &[(String, String)], text: &str) {
    let typed = text.strip_prefix('/').filter(|rest| !rest.contains(char::is_whitespace));
    let matches: Vec<CommandItem> = match typed {
        Some(prefix) if !commands.is_empty() => commands
            .iter()
            .filter(|(command, _)| command.starts_with(prefix))
            .map(|(command, description)| CommandItem {
                name: format!("/{command}").into(),
                description: description.clone().into(),
            })
            .collect(),
        _ => Vec::new(),
    };
    with_app(|app| {
        let Some(ui) = app.ui.upgrade() else { return };
        let open = !matches.is_empty();
        ui.set_command_items(ModelRc::from(Rc::new(VecModel::from(matches))));
        ui.set_commands_open(open);
    });
}

/// Puts the picked command in the field and closes the list.
pub fn pick(index: i32) {
    with_app(|app| {
        let Some(ui) = app.ui.upgrade() else { return };
        let items = ui.get_command_items();
        let Some(item) = items.row_data(index as usize) else { return };
        let text = format!("{} ", item.name);
        ui.set_commands_open(false);
        ui.invoke_set_composer(text.as_str().into());
        ui.invoke_focus_composer();
    });
}
