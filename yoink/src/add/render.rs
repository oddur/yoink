//! Variable substitution for template files, dest paths, secret
//! names, and notes. Uses minijinja so manifest authors get
//! `{{ var | upper }}`, `{{ var | default("x") }}`, `{% if … %}` for
//! free.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use minijinja::Environment;

use super::manifest::{ConnectionSpec, FileSpec, SecretSpec, TemplateManifest};

/// One file to write to disk after rendering.
#[derive(Debug, Clone)]
pub struct RenderedFile {
    pub dest: PathBuf,
    pub contents: String,
}

/// One secret to seal after rendering. The value is generated locally
/// (typically `random:N`) — never carried across the wire.
#[derive(Debug, Clone)]
pub struct RenderedSecret {
    pub name: String,
    pub value: String,
}

/// Result of rendering a manifest against a variable map.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub files: Vec<RenderedFile>,
    pub secrets: Vec<RenderedSecret>,
    pub notes: Option<String>,
    pub include_glob: Option<String>,
    pub connection: Option<ConnectionSpec>,
}

/// Render every output the manifest declares.
///
/// `template_root` is the directory containing `template.yaml` — file
/// `template:` paths are resolved relative to it.
pub fn render(
    manifest: &TemplateManifest,
    template_root: &Path,
    variables: &BTreeMap<String, String>,
) -> Result<Rendered> {
    let env = build_env();
    let ctx = ctx_value(variables);

    let mut files = Vec::with_capacity(manifest.files.len());
    for file in &manifest.files {
        files.push(render_file(&env, template_root, file, &ctx)?);
    }

    let mut secrets = Vec::with_capacity(manifest.secrets.len());
    for spec in &manifest.secrets {
        secrets.push(render_secret(&env, spec, &ctx)?);
    }

    let notes = manifest
        .notes
        .as_deref()
        .map(|n| render_string(&env, "<notes>", n, &ctx))
        .transpose()?;

    let include_glob = manifest
        .include_glob
        .as_deref()
        .map(|g| render_string(&env, "<include_glob>", g, &ctx))
        .transpose()?;

    let connection = manifest
        .connection
        .as_ref()
        .map(|c| render_connection(&env, c, &ctx))
        .transpose()?;

    Ok(Rendered {
        files,
        secrets,
        notes,
        include_glob,
        connection,
    })
}

fn render_connection(
    env: &Environment<'_>,
    spec: &ConnectionSpec,
    ctx: &minijinja::Value,
) -> Result<ConnectionSpec> {
    let render_map = |label: &str, m: &BTreeMap<String, String>| -> Result<BTreeMap<String, String>> {
        m.iter()
            .map(|(k, v)| {
                let key = render_string(env, &format!("{label}.key({k})"), k, ctx)?;
                let val = render_string(env, &format!("{label}.value({k})"), v, ctx)?;
                Ok((key, val))
            })
            .collect()
    };

    let depends_on = spec
        .depends_on
        .iter()
        .enumerate()
        .map(|(i, s)| render_string(env, &format!("connection.depends_on[{i}]"), s, ctx))
        .collect::<Result<Vec<_>>>()?;

    Ok(ConnectionSpec {
        env: render_map("connection.env", &spec.env)?,
        env_from_secrets: render_map("connection.env_from_secrets", &spec.env_from_secrets)?,
        depends_on,
    })
}

fn build_env<'a>() -> Environment<'a> {
    let mut env = Environment::new();
    // Tighten templating: undefined → error, so a typo'd `{{ srvice_name }}`
    // surfaces immediately instead of producing empty strings in YAML.
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env
}

fn ctx_value(variables: &BTreeMap<String, String>) -> minijinja::Value {
    let map: BTreeMap<String, minijinja::Value> = variables
        .iter()
        .map(|(k, v)| (k.clone(), minijinja::Value::from(v.as_str())))
        .collect();
    minijinja::Value::from_serialize(&map)
}

fn render_string(
    env: &Environment<'_>,
    label: &str,
    source: &str,
    ctx: &minijinja::Value,
) -> Result<String> {
    let tmpl = env
        .template_from_str(source)
        .with_context(|| format!("compile {label}"))?;
    tmpl.render(ctx).with_context(|| format!("render {label}"))
}

fn render_file(
    env: &Environment<'_>,
    template_root: &Path,
    spec: &FileSpec,
    ctx: &minijinja::Value,
) -> Result<RenderedFile> {
    // Re-validate the relative-path constraint at render time: the
    // manifest validator caught literal `..`, but a malicious template
    // could write `dest: "{{ escape }}"` and supply `escape = "../x"`.
    let dest_str = render_string(env, &format!("dest({})", spec.dest), &spec.dest, ctx)?;
    reject_escape(&dest_str, "dest")?;
    let dest = PathBuf::from(dest_str);

    let template_path = template_root.join(&spec.template);
    if !template_path.starts_with(template_root) {
        anyhow::bail!(
            "template `{}` path escapes the template root",
            spec.template
        );
    }
    let template_text = std::fs::read_to_string(&template_path).with_context(|| {
        format!("read template body {}", template_path.display())
    })?;
    let body = render_string(env, &spec.template, &template_text, ctx)?;

    Ok(RenderedFile { dest, contents: body })
}

fn render_secret(
    env: &Environment<'_>,
    spec: &SecretSpec,
    ctx: &minijinja::Value,
) -> Result<RenderedSecret> {
    let name = render_string(env, &format!("secret({})", spec.name), &spec.name, ctx)?;
    let value = generate_secret(&spec.generate)?;
    Ok(RenderedSecret { name, value })
}

fn generate_secret(spec: &str) -> Result<String> {
    let n = spec
        .strip_prefix("random:")
        .ok_or_else(|| anyhow::anyhow!("unknown generator `{spec}` (expected `random:N`)"))?
        .parse::<usize>()
        .with_context(|| format!("parse byte count from `{spec}`"))?;
    if !(8..=512).contains(&n) {
        anyhow::bail!("random:N must be between 8 and 512 (got {n})");
    }
    let mut buf = vec![0u8; n];
    fill_random(&mut buf)?;
    Ok(base32_encode(&buf))
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    use std::io::Read;
    // /dev/urandom is non-blocking and never fails on a healthy unix.
    // Bubble up the error so a misconfigured CI sandbox surfaces a
    // clear failure rather than a zero-filled "secret".
    let mut f = std::fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    f.read_exact(buf).context("read /dev/urandom")?;
    Ok(())
}

const B32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buf: u64 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        buf = (buf << 8) | u64::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buf >> bits) & 0b11111) as usize;
            out.push(B32_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buf << (5 - bits)) & 0b11111) as usize;
        out.push(B32_ALPHABET[idx] as char);
    }
    out
}

fn reject_escape(p: &str, field: &str) -> Result<()> {
    if super::manifest::escapes_root(Path::new(p)) {
        anyhow::bail!("rendered {field} `{p}` escapes the repo root");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn manifest_with(files: Vec<FileSpec>, secrets: Vec<SecretSpec>) -> TemplateManifest {
        TemplateManifest {
            name: "t".into(),
            kind: super::super::manifest::TemplateKind::Accessory,
            description: None,
            yoink_min_version: None,
            variables: Vec::new(),
            files,
            secrets,
            include_glob: None,
            notes: None,
            connection: None,
        }
    }

    #[test]
    fn renders_simple_template() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.tmpl"), "hello {{ name }}").unwrap();
        let manifest = manifest_with(
            vec![FileSpec {
                dest: "out/{{ name }}.txt".into(),
                template: "a.tmpl".into(),
            }],
            vec![],
        );
        let mut vars = BTreeMap::new();
        vars.insert("name".into(), "world".into());
        let r = render(&manifest, tmp.path(), &vars).unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].dest.to_string_lossy(), "out/world.txt");
        assert_eq!(r.files[0].contents, "hello world");
    }

    #[test]
    fn upper_filter_works_in_secret_names() {
        let manifest = manifest_with(
            vec![],
            vec![SecretSpec {
                name: "{{ name | upper }}_PASSWORD".into(),
                generate: "random:16".into(),
            }],
        );
        let tmp = TempDir::new().unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("name".into(), "postgres".into());
        let r = render(&manifest, tmp.path(), &vars).unwrap();
        assert_eq!(r.secrets[0].name, "POSTGRES_PASSWORD");
        // 16 bytes → ceil(128/5) = 26 base32 chars
        assert!(r.secrets[0].value.len() >= 25);
        assert!(r.secrets[0].value.chars().all(|c| B32_ALPHABET.contains(&(c as u8))));
    }

    #[test]
    fn undefined_variable_errors() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.tmpl"), "{{ missing }}").unwrap();
        let manifest = manifest_with(
            vec![FileSpec {
                dest: "out.txt".into(),
                template: "a.tmpl".into(),
            }],
            vec![],
        );
        let vars = BTreeMap::new();
        let err = render(&manifest, tmp.path(), &vars).unwrap_err();
        // Should mention undefined or rendering failure
        assert!(
            err.chain().any(|e| {
                let s = e.to_string();
                s.contains("undefined") || s.contains("missing")
            }),
            "got: {err:?}"
        );
    }

    #[test]
    fn rejects_rendered_dest_with_parent() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.tmpl"), "x").unwrap();
        let manifest = manifest_with(
            vec![FileSpec {
                dest: "{{ escape }}".into(),
                template: "a.tmpl".into(),
            }],
            vec![],
        );
        let mut vars = BTreeMap::new();
        vars.insert("escape".into(), "../oops.txt".into());
        let err = render(&manifest, tmp.path(), &vars).unwrap_err();
        assert!(err.to_string().contains("escapes"));
    }

    #[test]
    fn random_secret_has_unique_values() {
        let s1 = generate_secret("random:32").unwrap();
        let s2 = generate_secret("random:32").unwrap();
        assert_ne!(s1, s2);
        assert!(s1.len() >= 50);
    }

    #[test]
    fn base32_encode_known_vector() {
        // RFC 4648 test vector: "foobar" → "MZXW6YTBOI======" (we don't pad)
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn rejects_unknown_generator() {
        assert!(generate_secret("uuid:v4").is_err());
        assert!(generate_secret("random:abc").is_err());
        assert!(generate_secret("random:0").is_err());
        assert!(generate_secret("random:99999").is_err());
    }
}
