use std::borrow::Cow;

use gpui_kit::assets::{Assets as ComponentAssets, icon_assets};
use gpui_kit::{AssetSource, Result, SharedString};

icon_assets!(ExtraIcons, [Monitor]);

pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        ComponentAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = ComponentAssets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::Assets;
    use gpui_kit::AssetSource as _;

    #[test]
    fn the_shell_bundles_its_history_picture_icon() {
        let path = gpui_kit::assets::IconName::Monitor.path();
        let svg = Assets
            .load(&path)
            .expect("the history picture icon is bundled")
            .expect("the history picture icon has data");
        assert!(svg.starts_with(b"<svg"));
    }

    #[test]
    fn the_shell_keeps_the_component_assets_alongside_its_extra_icon() {
        let paths = Assets.list("icons/").expect("icon paths");
        for icon in [
            gpui_kit::assets::IconName::Bot,
            gpui_kit::assets::IconName::Monitor,
        ] {
            let path = icon.path();
            assert_eq!(paths.iter().filter(|listed| **listed == path).count(), 1);
            assert!(
                Assets
                    .load(&path)
                    .expect("bundled icon")
                    .expect("icon data")
                    .starts_with(b"<svg")
            );
        }
        assert_eq!(
            Assets.list("absent/").expect("absent prefix"),
            Vec::<gpui_kit::SharedString>::new()
        );
    }
}
