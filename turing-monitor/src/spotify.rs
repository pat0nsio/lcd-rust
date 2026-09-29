// SPDX-License-Identifier: GPL-3.0-or-later
//! Lo que suena ahora mismo, leido del reproductor por MPRIS.
//!
//! Se llama a `busctl` en vez de enlazar una crate de D-Bus: es un proceso
//! por refresco contra un bus que ya esta ahi, frente a un arbol de
//! dependencias mas grande que el resto del binario.
//!
//! ponytail: un proceso por fotograma. Si alguna vez molesta, lo que toca es
//! una conexion D-Bus persistente con `PropertiesChanged`, no un cache.

use std::process::{Command, Stdio};

use turing_lcd::Image;

/// Nombre del bus del reproductor. MPRIS es un estandar, pero el tema es de
/// Spotify y ese es el unico bus que se consulta.
const BUS: &str = "org.mpris.MediaPlayer2.spotify";

pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// "Playing", "Paused" o "Stopped" tal cual lo da MPRIS.
    pub status: String,
    /// Segundos; `length` es 0 cuando el reproductor no lo publica.
    pub position: f64,
    pub length: f64,
    /// URL de la caratula, vacia si la pista no trae.
    pub art_url: String,
}

pub fn poll() -> Option<NowPlaying> {
    let out = Command::new("busctl")
        .args([
            "--user",
            "--json=short",
            "call",
            BUS,
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties",
            "GetAll",
            "s",
            "org.mpris.MediaPlayer2.Player",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        // Sin Spotify abierto no hay bus: no es un error, es que no suena nada.
        return None;
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// Extrae los campos que interesan del JSON de `busctl`.
///
/// No hay parser de JSON en el arbol de dependencias y no merece la pena
/// anadir uno: las claves de MPRIS son unicas dentro de la respuesta, asi que
/// basta con buscar cada una y leer su `"data"`.
fn parse(json: &str) -> Option<NowPlaying> {
    let status = text(json, "PlaybackStatus")?;
    Some(NowPlaying {
        title: text(json, "xesam:title").unwrap_or_default(),
        artist: text(json, "xesam:artist").unwrap_or_default(),
        album: text(json, "xesam:album").unwrap_or_default(),
        status,
        // MPRIS cuenta en microsegundos.
        position: num(json, "Position").unwrap_or(0.0) / 1e6,
        length: num(json, "mpris:length").unwrap_or(0.0) / 1e6,
        art_url: text(json, "mpris:artUrl").unwrap_or_default(),
    })
}

/// El valor en crudo que sigue a `"clave": { ..., "data": `.
fn data_of<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\":"))?;
    let rest = &json[at..];
    let start = rest.find("\"data\":")? + "\"data\":".len();
    Some(rest[start..].trim_start())
}

fn text(json: &str, key: &str) -> Option<String> {
    let v = data_of(json, key)?;
    // Los campos `as` (xesam:artist) llegan como lista; vale el primero.
    let v = v.strip_prefix('[').unwrap_or(v).trim_start();
    let mut chars = v.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' | 't' | 'r' => out.push(' '),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let n = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(n).unwrap_or('?'));
                }
                esc => out.push(esc),
            },
            c => out.push(c),
        }
    }
    None
}

fn num(json: &str, key: &str) -> Option<f64> {
    let v = data_of(json, key)?;
    let end = v
        .find(|c: char| !matches!(c, '0'..='9' | '-' | '+' | '.' | 'e' | 'E'))
        .unwrap_or(v.len());
    v[..end].parse().ok()
}

// ---------------------------------------------------------------- caratula

/// Descarga y descodifica la caratula.
///
/// `curl | djpeg`: dos procesos diminutos una vez por cancion, contra una
/// crate de HTTP mas otra de JPEG que estarian residentes siempre. `djpeg`
/// va con libjpeg-turbo, que ya esta en cualquier escritorio.
///
/// `-scale 1/2` descodifica a la mitad dentro del propio JPEG — la mitad de
/// los coeficientes y la cuarta parte de memoria — que es de largo la parte
/// cara de esto. Los 640x640 de Spotify salen a 320 y el que escala al hueco
/// del tema es el renderizador.
pub fn art(url: &str) -> Option<Image> {
    if !url.starts_with("https://") {
        return None;
    }
    let mut dl = Command::new("curl")
        // `--proto =https` acota a donde puede apuntar una URL que llega por
        // el bus: nada de file:// ni de redirecciones a otro esquema.
        .args(["-sfL", "--proto", "=https", "--max-time", "6", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let pipe = dl.stdout.take()?;
    let out = Command::new("djpeg")
        .args(["-scale", "1/2", "-ppm"])
        .stdin(pipe)
        .stderr(Stdio::null())
        .output()
        .ok();
    let _ = dl.wait();
    parse_ppm(&out?.stdout)
}

/// PPM binario (P6), que es lo que escupe `djpeg`.
fn parse_ppm(d: &[u8]) -> Option<Image> {
    if !d.starts_with(b"P6") {
        return None;
    }
    let mut i = 2;
    let w = ppm_number(d, &mut i)?;
    let h = ppm_number(d, &mut i)?;
    let max = ppm_number(d, &mut i)?;
    // Un solo byte de separacion entre la cabecera y los pixeles.
    i += 1;
    let n = w as usize * h as usize * 3;
    if max != 255 || w == 0 || h == 0 || d.len() < i + n {
        return None;
    }
    Some(Image {
        w,
        h,
        px: d[i..i + n].to_vec(),
    })
}

fn ppm_number(d: &[u8], i: &mut usize) -> Option<u16> {
    loop {
        while *i < d.len() && d[*i].is_ascii_whitespace() {
            *i += 1;
        }
        if *i < d.len() && d[*i] == b'#' {
            while *i < d.len() && d[*i] != b'\n' {
                *i += 1;
            }
            continue;
        }
        break;
    }
    let start = *i;
    while *i < d.len() && d[*i].is_ascii_digit() {
        *i += 1;
    }
    std::str::from_utf8(&d[start..*i]).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Respuesta real de `busctl --user --json=short ... GetAll`, recortada.
    const SAMPLE: &str = r#"{"type":"a{sv}","data":[{"PlaybackStatus":{"type":"s","data":"Playing"},"Metadata":{"type":"a{sv}","data":{"mpris:trackid":{"type":"s","data":"/com/spotify/track/3CR"},"mpris:length":{"type":"t","data":255175000},"xesam:album":{"type":"s","data":"Nola"},"xesam:artist":{"type":"as","data":["DOWN","Otro"]},"xesam:title":{"type":"s","data":"Temptation's \"Wings\""}}},"Position":{"type":"x","data":160774000},"CanSeek":{"type":"b","data":true}}]}"#;

    #[test]
    fn reads_the_fields_a_theme_shows() {
        let n = parse(SAMPLE).expect("should parse");
        assert_eq!(n.title, "Temptation's \"Wings\"");
        assert_eq!(n.artist, "DOWN");
        assert_eq!(n.album, "Nola");
        assert_eq!(n.status, "Playing");
        assert_eq!(n.position.round(), 161.0);
        assert_eq!(n.length.round(), 255.0);
    }

    #[test]
    fn escapes_come_back_as_characters() {
        let json = r#"{"xesam:title":{"type":"s","data":"café — uno\ndos"}}"#;
        assert_eq!(text(json, "xesam:title").unwrap(), "café — uno dos");
    }

    #[test]
    fn ppm_header_survives_comments_and_odd_spacing() {
        let mut d = b"P6\n# hecho por djpeg\n2  1\n255\n".to_vec();
        d.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let img = parse_ppm(&d).expect("should parse");
        assert_eq!((img.w, img.h), (2, 1));
        assert_eq!(img.px, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_truncated_ppm_is_rejected_instead_of_panicking() {
        assert!(parse_ppm(b"P6\n2 2\n255\nxyz").is_none());
        assert!(parse_ppm(b"not a ppm").is_none());
    }

    #[test]
    fn a_reply_without_a_player_is_not_a_track() {
        assert!(parse("{}").is_none());
    }
}
