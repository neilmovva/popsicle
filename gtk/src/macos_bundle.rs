use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process,
};

const RESOURCE_TOKEN: &str = "@POPSICLE_RESOURCES@";

pub fn configure() -> io::Result<()> {
    let executable = env::current_exe()?;
    let Some(contents) = executable.parent().and_then(Path::parent) else {
        return Ok(());
    };
    let resources = contents.join("Resources");

    // A development build has no adjacent app-bundle resources.
    if !resources.join("lib/gdk-pixbuf-2.0/2.10.0/loaders.cache.in").is_file() {
        return Ok(());
    }

    let cache_dir = user_cache_dir();
    fs::create_dir_all(&cache_dir)?;

    let pixbuf_cache = expand_cache_template(
        &resources,
        "lib/gdk-pixbuf-2.0/2.10.0/loaders.cache.in",
        &cache_dir.join("gdk-pixbuf-loaders.cache"),
    )?;
    let immodules_cache = expand_cache_template(
        &resources,
        "lib/gtk-3.0/3.0.0/immodules.cache.in",
        &cache_dir.join("gtk-immodules.cache"),
    )?;

    env::set_var("GDK_PIXBUF_MODULE_FILE", pixbuf_cache);
    env::set_var("GDK_PIXBUF_MODULEDIR", contents.join("PlugIns"));
    env::set_var("GTK_IM_MODULE_FILE", immodules_cache);
    env::set_var("GTK_DATA_PREFIX", &resources);
    env::set_var("GSETTINGS_SCHEMA_DIR", resources.join("share/glib-2.0/schemas"));
    env::set_var("XDG_DATA_DIRS", resources.join("share"));
    env::set_var("FONTCONFIG_FILE", resources.join("etc/fonts/fonts.conf"));
    env::set_var("FONTCONFIG_PATH", resources.join("etc/fonts"));

    Ok(())
}

fn user_cache_dir() -> PathBuf {
    env::var_os("HOME").map_or_else(
        || env::temp_dir().join("com.system76.Popsicle"),
        |home| PathBuf::from(home).join("Library/Caches").join("com.system76.Popsicle"),
    )
}

fn expand_cache_template(
    resources: &Path,
    template_relative_path: &str,
    cache_path: &Path,
) -> io::Result<PathBuf> {
    let template = fs::read_to_string(resources.join(template_relative_path))?;
    let resource_path = resources.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Popsicle.app has a non-Unicode path")
    })?;
    let escaped_resource_path = resource_path.replace('\\', "\\\\").replace('"', "\\\"");
    let expanded = template.replace(RESOURCE_TOKEN, &escaped_resource_path);
    let temporary_path = cache_path.with_extension(format!("tmp-{}", process::id()));

    fs::write(&temporary_path, expanded)?;
    fs::rename(&temporary_path, cache_path)?;

    Ok(cache_path.to_path_buf())
}
