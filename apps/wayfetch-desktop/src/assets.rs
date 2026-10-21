//! The shared web UI, embedded from `ui/src` at compile time.
//!
//! Only the entry page differs from `wayfetchd`'s: the window boots
//! `shell/desktop.html`, also used as the fallback for unknown paths after a
//! reload.

use std::borrow::Cow;

use tauri::utils::assets::{AssetKey, AssetsIter, CspHash};
use tauri::{App, Assets, Runtime};

/// The page every navigation lands on.
const PAGE: &str = "/index.html";
/// The desktop bootstrap that replaces it.
const DESKTOP_PAGE: &str = "/shell/desktop.html";

/// The embedded UI, with the desktop bootstrap as its page.
pub struct DesktopAssets<R: Runtime>(pub Box<dyn Assets<R>>);

impl<R: Runtime> DesktopAssets<R> {
    fn key<'a>(key: &'a AssetKey) -> Cow<'a, AssetKey> {
        if key.as_ref() == PAGE {
            Cow::Owned(AssetKey::from(DESKTOP_PAGE))
        } else {
            Cow::Borrowed(key)
        }
    }
}

impl<R: Runtime> Assets<R> for DesktopAssets<R> {
    fn setup(&self, app: &App<R>) {
        self.0.setup(app);
    }

    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        self.0.get(&Self::key(key))
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        self.0.iter()
    }

    fn csp_hashes(&self, html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        self.0.csp_hashes(&Self::key(html_path))
    }
}
