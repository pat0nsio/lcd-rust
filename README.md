# turing-lcd-rs

Reescritura en Rust del driver y monitor de sistema para paneles
**Turing Smart Screen rev. A / UsbMonitor** (USB `1a86:5722`, serie
`USB35INCHIPSV2`), portada desde
[mathoudebine/turing-smart-screen-python](https://github.com/mathoudebine/turing-smart-screen-python).

Dos crates:

| Crate | Qué es |
|---|---|
| `turing-lcd` | Librería: protocolo serie, framebuffer, texto TTF y widgets |
| `turing-monitor` | Binario: monitor de sistema listo para usar |

## Hardware objetivo

Detectado en esta máquina y para el que se escribió el port:

```
1a86:5722  Turing UsbMonitor  serial USB35INCHIPSV2
CDC-ACM, 115200 8N1 con control de flujo por hardware (RTS/CTS)
320x480 RGB565, /dev/ttyACM0
```

La librería también reconoce las sub-revisiones UsbMonitor 5" (480x800) y
7" (600x1024) mediante el comando `HELLO`, igual que el original.

## Compilar

```sh
cargo build --release
# binario en target/release/turing-monitor
```

Dependencias: `serialport` (sin libudev), `fontdue`, `libc`, y `serde`+`toml`
para los temas. Las fuentes Roboto Mono y el tema por defecto van incrustados
en el binario, que por tanto funciona sin ningún fichero externo.

## Permisos

`/dev/ttyACM0` pertenece a `root:uucp`, así que hacen falta **los dos pasos**
(ya aplicados en esta máquina):

```sh
# 1. Regla udev: fija grupo y modo, y crea el enlace estable /dev/turing-lcd
sudo cp packaging/99-turing-smart-screen.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add /sys/class/tty/ttyACM0

# 2. Pertenencia al grupo (surte efecto al volver a iniciar sesión)
sudo usermod -aG uucp $USER
```

Dos detalles que costaron un rato aquí:

- `udevadm trigger` a secas emite un evento `change`, y el tag `uaccess` solo
  se procesa en `add`. Sin `--action=add` la regla parece no aplicarse.
- Aun con `uaccess`, logind **no** concedió ACL a este tty (los CDC-ACM no
  suelen quedar asignados a un asiento), así que el grupo `uucp` no es
  opcional. Para probar sin cerrar sesión: `echo "comando" | newgrp uucp`.

## Uso

```sh
turing-monitor                  # monitor, refresco 1 s
turing-monitor detect           # puerto, modelo y resolución detectados
turing-monitor --once           # dibuja un fotograma y sale
turing-monitor --stats          # bytes enviados por fotograma (a stderr)
turing-monitor --dump vista.ppm # renderiza a fichero, sin tocar el hardware
turing-monitor --check-theme    # valida el tema y mide si cabe en la pantalla
turing-monitor --list-metrics   # lista las metricas disponibles y su valor
turing-monitor -t mi-tema.toml  # usa otro tema
turing-monitor brightness 40
turing-monitor on | off | clear | reset
```

Opciones: `-c/--config`, `-p/--port`, `-b/--brightness`, `-r/--refresh`,
`-t/--theme`.

Configuración en `~/.config/turing-monitor.conf`; ver
`turing-monitor.conf.example`. Formato `clave = valor`, sin YAML. Solo
contiene lo que toca al dispositivo y a las fuentes de datos: el aspecto vive
en el tema.

Para arranque automático hay una unidad de usuario en
`packaging/turing-monitor.service`.

## Qué muestra

Hostname y uptime · CPU (uso global, barra por hilo, histórico, frecuencia
media y Tctl) · GPU AMD (uso, VRAM y temperatura vía sysfs `amdgpu`) ·
RAM y swap · ocupación de disco · load average.

Hay además una sección de red desactivada en el tema por defecto; se
recupera descomentando su bloque en `themes/default.toml`.

## Temas

Un tema es un fichero TOML que describe una pila de secciones. **Nada lleva
coordenadas absolutas**: las secciones fluyen de arriba abajo y las filas se
apilan dentro de cada una, así que el mismo tema sirve para 320x480 o para
cualquier otro tamaño de panel.

```toml
[palette]
cpu  = "#58a6ff"
warn = "#f85149"

[[section]]
title = "CPU"
value = "cpu.usage | percent"
color = "cpu -> warn @ 0.7"       # degradado segun carga
rows = [
  { text = "{cpu.model}  {cpu.freq|ghz}", style = "dim" },
  { bar   = "cpu.usage", height = 10 },
  { cores = "cpu.cores", height = 14 },
  { plot  = "cpu.usage", height = 34 },
]
```

**Paleta.** Los colores se declaran una vez en `[palette]` y se referencian
por nombre; también valen literales `#rrggbb` o `r,g,b`. El nombre `panel`
es especial: es el fondo por defecto de barras y gráficos.

**Colores según carga.** `"cpu -> warn @ 0.7"` mantiene el color base hasta
el 70% y de ahí interpola hasta `warn`. Con `color_from` se puede alimentar
el degradado desde otra métrica distinta de la mostrada — así la temperatura,
que no es una fracción, usa `cpu.temp_ratio`.

**Tipos de fila.** `text` (con `right` para la columna derecha), `bar`,
`cores` (una barra por elemento de una serie), `plot` (histórico; acepta una
lista de métricas para varias series en un mismo eje), `rule` y `gap`.
Ajustes: `height`, `size`, `style` (`normal`/`bold`/`dim`), `color`,
`fill`, `track`, `max`, `scale` (`unit` o `auto`).

**Métricas por nombre.** Los widgets se enlazan por cadena — `cpu.usage`,
`gpu.temp`, `mem.used` — resueltas contra una tabla. Añadir una métrica es
insertarla en `metrics.rs`; el motor de dibujo no necesita enterarse.
`--list-metrics` imprime las 32 disponibles con su valor actual.

**Formateadores.** `metrica | formateador`, con `percent`, `celsius`,
`bytes`, `rate`, `ghz`, `mhz`, `uptime`, `int`, `float1`, `float2`.

**Plantillas.** Entre llaves se mezcla texto literal y métricas:
`"VRAM {gpu.vram_used|bytes} / {gpu.vram_total|bytes}"`. Sin llaves, el valor
se interpreta como una referencia a métrica; los `title` son al revés, texto
literal salvo las partes entre llaves.

**Secciones y filas condicionales.** `require = "gpu.usage"` descarta la
sección entera si esa métrica no existe en la máquina — sin GPU AMD no queda
hueco. `require` también funciona por fila: así las líneas de swap
desaparecen en un equipo sin swap.

**Anclaje.** `anchor = "bottom"` fija una sección al borde inferior; el resto
fluye desde arriba y se detiene antes de solaparla.

**Herramientas.** `--check-theme` valida el fichero, dice cuántos píxeles de
alto consume frente a los disponibles, y avisa de secciones que no caben —
que si no desaparecerían en silencio. `--dump fichero.ppm` renderiza sin
tener la pantalla conectada. Las claves desconocidas son un error, no un
campo ignorado: un `heigth` mal escrito se rechaza al arrancar.

### Diferencias con los temas del original

| | Original (YAML) | Aquí (TOML) |
|---|---|---|
| Posición | `X`/`Y` absolutos por widget | flujo vertical, sin coordenadas |
| Resolución | un tema por tamaño de panel | el mismo tema en cualquiera |
| Colores | literales repetidos por widget | paleta con nombres, una vez |
| Métricas | árbol fijo cableado en el código | tabla por nombre, ampliable |
| Errores | clave desconocida se ignora | clave desconocida es error |
| Tamaño | ~5 KB por tema, 78 temas ≈ 14.000 líneas | ~100 líneas |

## Usar como librería

```rust
use turing_lcd::{Display, Orientation};

let mut display = Display::open(None, Orientation::Portrait)?;
display.device_mut().set_brightness(60)?;
display.canvas().clear([0, 0, 0]);
// ...dibujar...
display.flush()?; // solo se envía lo que cambió
```

Ejemplo completo: `cargo run -p turing-lcd --example demo`.

## Diferencias con el original

**El envío es diferencial.** El original vuelve a enviar el bitmap de cada
widget en cada refresco. Aquí el canvas se compara contra una copia de lo que
el panel ya muestra y solo se transmiten las filas que cambiaron, agrupadas
en rectángulos con un criterio de coste (fusionar dos bandas solo si sale más
barato que emitir dos comandos).

El enlace es el cuello de botella, no la CPU. Medido en esta máquina:

| | Fotograma | Datos | Tiempo |
|---|---|---|---|
| Completo (primer refresco) | 153.600 px | 307 KB | 2.382 ms |
| Diferencial (régimen estable) | ~3.000 px | ~6 KB | ~55 ms |

Unas 50 veces menos datos. Es la diferencia entre no poder refrescar ni una
vez por segundo y poder hacerlo unas 18 veces.

**Sin hilos ni colas.** El original usa una cola de peticiones y varios hilos
de trabajo. Aquí el bucle es único y síncrono; el control de flujo por
hardware ya regula el ritmo.

**Sin asignaciones en régimen estable.** Un solo framebuffer reutilizado, un
buffer de codificación reutilizado, caché de glifos, y los lectores de `/proc`
reutilizan su `String`. El original crea una imagen PIL por widget y refresco.

**Métricas por sysfs directo**, en lugar de `psutil` + `GPUtil` +
`pyamdgpuinfo`.

Consumo medido con el monitor corriendo contra el panel: **9,9 MB de RSS
pico**, constante a lo largo de la ejecución — sin crecimiento entre
fotogramas. Binario de 1,05 MB y 31 dependencias transitivas; el tema se
parsea una vez al arrancar, así que `serde`/`toml` no aparecen en el consumo
en régimen. Comparado con las 25 dependencias directas que declara el
`requirements.txt` del original (numpy, Pillow, psutil, tkinter, pystray…).
No hay comparativa de consumo con el original porque no llegó a instalarse.

Volumen de código comparable: ~2.600 líneas de Rust frente a ~2.780 líneas
de los módulos Python equivalentes (rev. A + monitor + stats + config).

## Qué NO está portado

Alcance deliberado: tu pantalla y su protocolo. Queda fuera del port:

- Las demás revisiones de hardware (rev. B, C, D, WeAct, Kipye, TURZX USB).
  El armazón está: añadir una revisión es implementar otro módulo junto a
  `device.rs`.
- Los temas YAML del original y su editor gráfico (`theme-editor.py`). El
  sistema de temas de aquí es nuevo y no lee los `theme.yaml` existentes.
- El icono de bandeja del sistema y el asistente `configure.py`.
- GPU NVIDIA e Intel: solo hay lectura de `amdgpu` por sysfs, que es lo que
  tiene esta máquina.

## Estado de las pruebas

Probado contra el panel real:

- `detect` identifica el modelo como `Turing35` a 320x480. El panel no
  responde al comando `HELLO`, exactamente como documenta el original para la
  3.5" oficial.
- Fotograma completo y refresco continuo a 1 Hz, con las cifras de la tabla
  de arriba.
- 26 pruebas unitarias pasan (`cargo test`), incluidas las tramas de comando
  verificadas contra los valores que produce el código Python original, el
  parseo de temas y los formateadores.
- Layout validado además sin hardware, renderizando a PPM con `--dump`.

## Licencia

GPL-3.0-or-later. Es una obra derivada del proyecto de Matthieu Houdebine
(GPL-3.0): el protocolo y los valores de los comandos vienen de ahí.
Roboto Mono, en `assets/`, es Apache-2.0.
