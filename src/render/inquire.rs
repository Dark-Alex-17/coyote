use syntect::highlighting::{Highlighter, Theme};
use anyhow::Result;
use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet};
use syntect::parsing::Scope;
use crate::render::RenderOptions;

const DEFAULT_INQUIRE_PROMPT_THEME: Color = Color::DarkYellow;

pub fn prompt_theme<'a>(render_options: RenderOptions) -> Result<RenderConfig<'a>> {
	let theme = render_options.theme.as_ref();
	let mut render_config = RenderConfig::default();

	if let Some(theme_ref) = theme {
		let prompt_color = resolve_foreground(theme_ref, "markup.heading")?
			.unwrap_or(DEFAULT_INQUIRE_PROMPT_THEME);

		render_config.prompt = StyleSheet::new()
			.with_fg(prompt_color)
			.with_attr(Attributes::BOLD);
		render_config.selected_option = Some(
			render_config
				.selected_option
				.unwrap_or(render_config.option)
				.with_attr(
					render_config
						.selected_option
						.unwrap_or(render_config.option)
						.att
						| Attributes::BOLD,
				),
		);
		render_config.selected_checkbox = render_config
			.selected_checkbox
			.with_attr(render_config.selected_checkbox.style.att | Attributes::BOLD);
		render_config.option = render_config
			.option
			.with_attr(render_config.option.att | Attributes::BOLD);
	}

	Ok(render_config)
}

fn resolve_foreground(theme: &Theme, scope_str: &str) -> Result<Option<Color>> {
	let scope = Scope::new(scope_str)?;
	let style_mod = Highlighter::new(theme).style_mod_for_stack(&[scope]);
	let fg = style_mod.foreground.or(theme.settings.foreground);

	Ok(fg.map(|c| Color::Rgb {
		r: c.r,
		g: c.g,
		b: c.b,
	}))
}
