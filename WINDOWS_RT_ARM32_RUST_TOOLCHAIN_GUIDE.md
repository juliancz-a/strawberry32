# Rust en Windows RT ARM32
## Portabilidad, toolchain, Windows SDK y pruebas de compatibilidad

**Estado del proyecto:** compilación y ejecución ARM32 funcionales en hardware real.  
**Target:** `thumbv7a-pc-windows-msvc`  
**Bindings de Windows:** `windows-sys 0.61.2` personalizado (`vendor/windows-sys-0.61/` con patch `arm32-windows-sys-0.61.patch`)  
**TLS / Criptografía:** Schannel nativo de Windows RT (`native-tls` + `default-tls`, sin `ring`/`rustls`)  
**Aplicación:** RT-Stream ARM32 (GUI Win32 táctil nativa, Stremio + `librqbit 8.0` con aceleración Tegra 3/4 DXVA2)  
**Sistema objetivo:** Windows RT (ARM32)  
**Entorno de desarrollo:** Windows x64  
**Compilador:** Rust nightly + LLVM 23.1.1

---

# 1. Introducción

El objetivo de este proyecto es conseguir compilar aplicaciones escritas en Rust para Windows RT ARM32 y ejecutarlas directamente en dispositivos como la Microsoft Surface RT.

Windows RT presenta una particularidad importante: utiliza procesadores ARM de 32 bits, pero su entorno de escritorio y sus APIs Win32 tienen diferencias respecto de las versiones convencionales de Windows para x86 y x64.

Además, el ecosistema de Rust no ofrece soporte oficial completo para este target. Esto implica que no alcanza con seleccionar una arquitectura ARM: también es necesario adaptar parte del entorno de compilación, las bibliotecas de Windows y las declaraciones de las APIs.

El trabajo se divide en cuatro áreas:

1. Configurar una toolchain capaz de generar ejecutables Windows ARM32.
2. Preparar el Windows SDK y las bibliotecas necesarias para enlazar.
3. Adaptar `windows-sys 0.60.2` para exponer las estructuras y declaraciones Win32 necesarias en ARM32.
4. Desarrollar una suite de pruebas y ejecutarla en una Surface RT real.

El resultado es un entorno que permite generar ejecutables PE ARM32 desde una computadora Windows x64 y comprobar su funcionamiento en Windows RT.

---

# 2. Arquitectura general

El proceso de compilación y ejecución está dividido entre dos dispositivos:

- **Host de desarrollo:** computadora Windows x64 donde se ejecutan Rust, Cargo, Clang y LLD.
- **Dispositivo objetivo:** Microsoft Surface RT con Windows RT ARM32, donde se ejecuta el binario generado.

La toolchain no necesita ejecutarse en la Surface RT. Su función es generar código para la arquitectura de destino desde el equipo de desarrollo.

## 2.1. Flujo de compilación

```text
┌──────────────────────────────────────┐
│         HOST WINDOWS x64             │
│                                      │
│  Rust / Cargo                        │
│       │                              │
│       ▼                              │
│  Target: thumbv7a-pc-windows-msvc    │
│       │                              │
│       ├── windows-sys 0.60.2         │
│       │      personalizado           │
│       │                              │
│       ├── Windows SDK ARM32          │
│       │                              │
│       ▼                              │
│  Clang / LLD                         │
│       │                              │
│       ▼                              │
│  Ejecutable PE ARM32                 │
└───────────────┬──────────────────────┘
                │
                │ Copiar ejecutable
                ▼
┌──────────────────────────────────────┐
│         MICROSOFT SURFACE RT          │
│                                      │
│  Windows RT ARM32                    │
│       │                              │
│       ▼                              │
│  test_4.exe                          │
│       │                              │
│       ▼                              │
│  Ejecución de APIs Win32             │
└──────────────────────────────────────┘
```

Esta separación permite desarrollar y compilar normalmente desde una computadora moderna sin depender de las herramientas disponibles en Windows RT.

---

# 3. Target de Rust

El target utilizado es:

```text
thumbv7a-pc-windows-msvc
```

Sus componentes principales son:

| Componente | Descripción |
|---|---|
| `thumbv7a` | Arquitectura ARMv7 en modo Thumb |
| `pc-windows` | Plataforma Windows |
| `msvc` | ABI y modelo de enlace compatible con MSVC |

Se eligió este target porque corresponde al entorno ARM32 de Windows que se busca utilizar.

El target genera ejecutables ARM32, no ejecutables x86 ni ARM64.

## 3.1. Entorno de compilación

Versiones utilizadas durante las pruebas:

```text
rustc 1.100.0-nightly
LLVM 23.1.1
windows-sys 0.60.2
```

Toolchain Clang/LLD:

```text
E:\arm32-tools\rt2.1-23.1.1-rt2.1-arm32-win\
```

Ejecutables:

```text
bin\clang-cl.exe
bin\lld-link.exe
```

La toolchain utilizada incluye modificaciones para ARM32 Windows RT.

**Importante:** las herramientas Clang y LLD se ejecutan en el host Windows x64. No deben copiarse a la Surface RT esperando que funcionen allí como herramientas nativas.

---

# 4. Windows SDK y bibliotecas

Para generar ejecutables Windows no alcanza con compilar el código Rust. También es necesario enlazar las funciones externas que utiliza el programa.

En Windows, esto requiere bibliotecas de importación y otras bibliotecas del sistema.

Se preparó un SDK con las cabeceras y bibliotecas ARM32 necesarias.

## 4.1. Estructura del SDK

Ubicación:

```text
E:\arm32-tools\winsdk\
```

Directorios relevantes:

```text
winsdk/
├── sdk/
│   ├── include/
│   └── lib/
│       └── um/
│           └── aarch/
│
└── crt/
    ├── include/
    └── lib/
        └── aarch/
```

Bibliotecas utilizadas:

```text
E:\arm32-tools\winsdk\crt\lib\aarch
E:\arm32-tools\winsdk\sdk\lib\um\aarch
E:\arm32-tools\winsdk\sdk\lib\ucrt\aarch
```

La carpeta `aarch` contiene las bibliotecas correspondientes a ARM32.

Es importante no confundirlas con las bibliotecas `x86`, `x64` o `arm64`: aunque los nombres de las APIs sean iguales, los formatos binarios y las convenciones de llamada deben corresponder a la arquitectura de destino.

---

# 5. Configuración de Cargo

Se configuró Cargo para utilizar LLD como linker del target ARM32.

Archivo:

```text
.cargo/config.toml
```

Contenido:

```toml
[target.thumbv7a-pc-windows-msvc]
linker = 'E:\arm32-tools\rt2.1-23.1.1-rt2.1-arm32-win\bin\lld-link.exe'
```

Esto permite que Cargo utilice el linker correcto automáticamente al compilar para `thumbv7a-pc-windows-msvc`.

## 5.1. Configuración de RUSTFLAGS

Durante las pruebas se utilizaron los siguientes flags:

```powershell
$flags = @(
    "--cfg windows_raw_dylib",
    "-C target-feature=+crt-static",
    "-C link-arg=/SUBSYSTEM:CONSOLE,6.03",
    "-C link-arg=/LIBPATH:E:\arm32-tools\winsdk\crt\lib\aarch",
    "-C link-arg=/LIBPATH:E:\arm32-tools\winsdk\sdk\lib\um\aarch",
    "-C link-arg=/LIBPATH:E:\arm32-tools\winsdk\sdk\lib\ucrt\aarch"
)

$env:RUSTFLAGS = $flags -join " "
```

### Explicación de los parámetros

**`--cfg windows_raw_dylib`**

Activa el mecanismo de importación de símbolos mediante `raw-dylib`.

Fue necesario para resolver un problema de enlace relacionado con:

```text
windows.0.53.0.lib
```

Esta biblioteca estaba siendo solicitada por la configuración de `windows-targets 0.53.5`.

Al utilizar `windows_raw_dylib`, se evita depender de esa biblioteca de importación generada y se permite que Rust gestione las importaciones directamente.

**`-C target-feature=+crt-static`**

Solicita el enlace estático del runtime C correspondiente al target.

Esto ayuda a reducir la dependencia de componentes externos del runtime, aunque no significa que todas las dependencias del sistema queden incorporadas al ejecutable.

**`-C link-arg=/SUBSYSTEM:CONSOLE,6.03`**

Especifica el subsistema del ejecutable PE:

- `CONSOLE`: aplicación de consola.
- `6.03`: versión del subsistema declarada en el ejecutable.

Se utiliza para generar un ejecutable de consola con una versión de subsistema adecuada para el entorno objetivo.

**`-C link-arg=/LIBPATH:...`**

Agrega las ubicaciones donde LLD debe buscar las bibliotecas ARM32 del SDK y del CRT.

Sin estas rutas pueden aparecer errores de símbolos o bibliotecas no encontradas durante el enlace.

### Por qué se utiliza un array de PowerShell

Se construyen los flags mediante un array y luego se unen con espacios:

```powershell
$env:RUSTFLAGS = $flags -join " "
```

Esto evita errores al concatenar manualmente cadenas largas.

Durante las primeras pruebas hubo problemas por argumentos que terminaban concatenados incorrectamente, haciendo que una ruta y el siguiente argumento se interpretaran como un único parámetro.

---

# 6. Personalización de windows-sys 0.60.2

## 6.1. ¿Qué es windows-sys?

`windows-sys` es una biblioteca de Rust que proporciona bindings de bajo nivel para las APIs de Windows.

Permite utilizar funciones Win32 desde Rust sin tener que declarar manualmente todas las funciones externas, estructuras, constantes y tipos.

Algunos ejemplos:

- `MessageBoxW`
- `CreateFileW`
- `ReadFile`
- `WriteFile`
- `CreateProcessW`
- `WSAStartup`
- `ShellExecuteW`

La biblioteca está organizada en módulos y features que habilitan diferentes conjuntos de APIs.

## 6.2. ¿Por qué fue necesario modificarla?

El problema encontrado es que algunas estructuras de Windows están condicionadas mediante atributos `cfg` que solo contemplan determinadas arquitecturas.

Por ejemplo:

```rust
#[cfg(target_arch = "x86")]
```

o:

```rust
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "arm64ec",
    target_arch = "x86_64"
))]
```

En esos casos, una estructura puede tener una definición válida para x86 y otra para x64/ARM64, pero ninguna disponible para ARM32.

Esto genera errores como:

```text
error[E0425]: cannot find type `SHCREATEPROCESSINFOW` in this scope
```

La función Win32 está declarada, pero su tipo asociado no está disponible para la arquitectura seleccionada.

No significa necesariamente que la API no exista en Windows RT. Puede ser simplemente una limitación de las condiciones de compilación de los bindings.

## 6.3. Estrategia utilizada

En lugar de modificar toda la biblioteca o crear bindings completamente nuevos, se decidió trabajar sobre `windows-sys 0.60.2` y adaptar únicamente las declaraciones necesarias.

La estrategia consiste en:

1. Identificar el tipo o función que falta.
2. Buscar su declaración real dentro del código fuente.
3. Revisar los atributos `cfg`.
4. Comparar las variantes existentes para x86, x64 y ARM64.
5. Comprobar los campos, tipos, tamaños y alineación de la estructura.
6. Agregar ARM32 únicamente cuando la definición sea compatible.
7. Mantener las demás arquitecturas sin cambios.

Este enfoque reduce la cantidad de modificaciones y evita introducir incompatibilidades innecesarias.

**No se debe agregar `target_arch = "arm"` indiscriminadamente a todas las estructuras de 64 bits.** Cada tipo debe analizarse individualmente.

---

# 7. Estructuras adaptadas

Durante la adaptación se encontraron estructuras que no estaban disponibles para ARM32.

Se agregaron condiciones para incluir `target_arch = "arm"` en las variantes correspondientes.

Lista de estructuras modificadas:

| Módulo | Estructura |
|---|---|
| Networking / WinSock | `WSADATA` |
| Networking / WinSock | `SERVENT` |
| System / Delay Load | `DELAYLOAD_INFO` |
| UI / Shell | `APPBARDATA` |
| UI / Shell | `SHFILEOPSTRUCTA` |
| UI / Shell | `SHFILEOPSTRUCTW` |
| UI / Shell | `SHQUERYRBINFO` |
| UI / Shell | `NOTIFYICONIDENTIFIER` |
| UI / Shell | `ASSOCIATIONELEMENT` |
| UI / Shell | `SHSTOCKICONINFO` |
| UI / Shell | `SHELLEXECUTEINFOA` |
| UI / Shell | `NOTIFYICONDATAA` |
| UI / Shell | `NOTIFYICONDATAW` |
| UI / Shell | `SHFILEINFOA` |
| UI / Shell | `SHFILEINFOW` |
| UI / Shell | `SHCREATEPROCESSINFOW` |

La lista representa las estructuras identificadas y adaptadas durante este trabajo.

## 7.1. Caso particular: SHCREATEPROCESSINFOW

Esta estructura fue uno de los errores encontrados al habilitar las APIs de creación de procesos.

La declaración original tenía dos variantes:

### Variante x86

```rust
#[repr(C, packed(1))]
#[cfg(target_arch = "x86")]
```

### Variante x64 / ARM64

```rust
#[repr(C)]
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "arm64ec",
    target_arch = "x86_64"
))]
```

La variante x86 utiliza `packed(1)`, mientras que la variante de 64 bits utiliza `repr(C)`.

Para ARM32 se agregó esta condición a la variante de 32 bits:

```rust
#[cfg(any(target_arch = "x86", target_arch = "arm"))]
```

También se modificó la condición de compilación de su implementación `Default`.

El objetivo fue reutilizar la definición de 32 bits existente, manteniendo intacta la variante de 64 bits.

Este cambio permitió resolver el error de tipo faltante durante la compilación.

---

# 8. Integración de windows-sys personalizado en Cargo

Se utiliza una copia local de `windows-sys 0.60.2` para poder mantener las modificaciones.

Estructura:

```text
streaming-app-arm32/
├── .cargo/
│   └── config.toml
│
├── vendor/
│   └── windows-sys/
│       ├── Cargo.toml
│       └── src/
│
├── test/
│   └── test-windows-sys/
│       ├── Cargo.toml
│       └── src/
│           └── main.rs
│
└── Cargo.toml
```

En el `Cargo.toml` raíz se utiliza:

```toml
[patch.crates-io]
windows-sys = { path = "vendor/windows-sys" }
```

Esto indica a Cargo que debe utilizar la copia local de `windows-sys` en lugar de descargar la versión publicada en crates.io.

La ventaja es que las modificaciones se aplican a todos los paquetes del proyecto que dependan de esa versión compatible de `windows-sys`.

También permite mantener los cambios en control de versiones y reutilizarlos en otros proyectos.

---

# 9. Features utilizadas

`windows-sys` utiliza features para habilitar módulos y APIs específicos.

Para el proyecto de pruebas se configuraron las siguientes:

```toml
[dependencies]
windows-sys = { version = "0.60.2", features = [
    "Win32_Foundation",
    "Win32_UI_Shell",
    "Win32_UI_WindowsAndMessaging",
    "Win32_Networking_WinSock",
    "Win32_System_Threading",
    "Win32_System_IO",
    "Win32_Storage_FileSystem",
    "Win32_Security",
] }
```

## 9.1. ¿Por qué son necesarias?

| Feature | Funcionalidad |
|---|---|
| `Win32_Foundation` | Tipos básicos de Windows, handles y constantes fundamentales |
| `Win32_UI_Shell` | APIs de Shell |
| `Win32_UI_WindowsAndMessaging` | APIs de ventanas y mensajes |
| `Win32_Networking_WinSock` | APIs de sockets de Windows |
| `Win32_System_Threading` | Procesos, threads y sincronización |
| `Win32_System_IO` | Operaciones de entrada y salida |
| `Win32_Storage_FileSystem` | Funciones de sistema de archivos |
| `Win32_Security` | Tipos y declaraciones relacionados con seguridad y permisos |

Un detalle importante es que algunas funciones están condicionadas por features que no parecen intuitivamente relacionadas con ellas.

Por ejemplo:

- `CreateFileW` requiere `Win32_Security` en la declaración encontrada.
- `CreateProcessW` también requiere `Win32_Security`.
- `ReadFile` y `WriteFile` dependen de `Win32_System_IO`.

Por eso se inspeccionaron las condiciones `cfg(feature = ...)` directamente en los archivos de `windows-sys`, en lugar de asumir que una feature habilitaba automáticamente todas las APIs relacionadas.

---

# 10. Suite de pruebas ARM32

Se creó un ejecutable de prueba independiente para comprobar las capacidades fundamentales del entorno.

Nombre utilizado:

```text
test_4.exe
```

El programa se compila para ARM32 y se copia a la Surface RT para ejecutarlo directamente.

La suite verifica cuatro áreas:

1. Comunicación TCP.
2. Creación y sincronización de threads.
3. Operaciones de archivos mediante Win32.
4. Creación y espera de procesos.

El objetivo no es solamente comprobar que Rust compila, sino verificar que las funciones realmente trabajan en Windows RT.

---

# 11. Prueba TCP

Se utiliza:

```rust
std::net::{TcpListener, TcpStream}
```

La prueba crea un servidor TCP local y un cliente que se conecta a él.

El cliente envía:

```text
RT_ARM32_PING
```

El servidor recibe el mensaje y responde.

Salida observada:

```text
=== TCP TEST ===
Server received: RT_ARM32_PING
[PASS] TCP client/server communication
```

### Qué valida

- Creación de sockets TCP.
- Apertura de un listener.
- Conexión de un cliente.
- Envío de datos.
- Recepción de datos.
- Funcionamiento de la implementación de red de Rust sobre Windows RT.

### Importancia

Esta prueba es especialmente relevante para el objetivo final del proyecto, ya que una aplicación de streaming necesita comunicación de red confiable.

El resultado demuestra que la comunicación TCP básica mediante `std::net` funciona en el dispositivo.

No representa todavía una prueba de streaming continuo, rendimiento de red, UDP o transferencia de grandes cantidades de datos.

---

# 12. Prueba de threading

Se utiliza:

```rust
std::thread::spawn
```

La prueba crea un thread secundario, ejecuta una operación y devuelve un resultado al thread principal.

Resultado:

```text
=== THREADING TEST ===
Worker thread is running!
Worker returned: 42
[PASS] Rust thread creation and join
```

### Qué valida

- Creación de threads mediante la biblioteca estándar.
- Ejecución concurrente de código.
- Comunicación de resultados mediante `JoinHandle`.
- Espera y finalización del thread mediante `join()`.

### Importancia

El threading es necesario para una aplicación de streaming porque permite separar tareas como:

- Recepción de datos de red.
- Decodificación.
- Procesamiento de imágenes.
- Renderizado.
- Gestión de conexiones.

Esta prueba valida `std::thread`, pero no demuestra todavía el funcionamiento de todas las APIs Win32 de threading.

Por ejemplo, `CreateThread`, mutexes, eventos y semáforos todavía requieren pruebas independientes.

---

# 13. Prueba de archivos Win32

Esta prueba utiliza directamente las APIs de Windows:

```rust
CreateFileW
WriteFile
ReadFile
CloseHandle
GetLastError
GetTempPathW
```

Se eligió trabajar con las APIs Win32 en lugar de utilizar exclusivamente `std::fs` para verificar que los bindings personalizados funcionan correctamente.

## 13.1. Creación del archivo

Se utiliza `CreateFileW` para crear o sobrescribir un archivo.

La llamada emplea:

```rust
GENERIC_WRITE
FILE_SHARE_READ
CREATE_ALWAYS
FILE_ATTRIBUTE_NORMAL
```

La función recibe una ruta UTF-16, porque las APIs Win32 terminadas en `W` utilizan cadenas Unicode de 16 bits.

## 13.2. Escritura

Se utiliza:

```rust
WriteFile(...)
```

Para escribir el siguiente contenido:

```text
Windows RT ARM32 file test
```

Se verifica que:

- La función devuelva éxito.
- La cantidad de bytes escritos coincida con el tamaño esperado.

## 13.3. Lectura

Después se vuelve a abrir el archivo utilizando:

```rust
GENERIC_READ
OPEN_EXISTING
```

Se lee su contenido mediante:

```rust
ReadFile(...)
```

Finalmente, se comparan los bytes leídos con los datos originales.

## 13.4. Problema encontrado: permisos y rutas

Durante las primeras pruebas, `CreateFileW` fallaba con:

```text
CreateFileW failed: 5
```

El código 5 corresponde a:

```text
ERROR_ACCESS_DENIED
```

Inicialmente se intentó ejecutar el programa desde `%TEMP%`, pero eso no cambiaba la ruta utilizada por el código.

La ruta original era:

```text
C:\rt_arm32_test.txt
```

Por lo tanto, aunque el ejecutable se encontrara en una carpeta temporal, seguía intentando escribir en la raíz de `C:\`.

Después se cambió la ruta y se encontró otro error:

```text
GetLastError: 3 (0x3)
```

Que corresponde a:

```text
ERROR_PATH_NOT_FOUND
```

Esto indicó que la ruta especificada no existía o que alguno de sus directorios no estaba disponible.

## 13.5. Solución: GetTempPathW

En lugar de codificar manualmente una ruta, se decidió obtener el directorio temporal mediante la API de Windows:

```rust
GetTempPathW
```

Esta función devuelve la ubicación temporal configurada para el entorno actual.

De esta forma, la aplicación no necesita asumir que el perfil del usuario se encuentra en una ruta específica.

La ruta obtenida se utiliza para construir el nombre del archivo:

```text
<directorio temporal>\rt_arm32_test.txt
```

También se verificó que la feature correspondiente y el import de `GetTempPathW` estuvieran disponibles.

**Resultado:** después de utilizar la ruta temporal obtenida dinámicamente, el problema de creación del archivo quedó resuelto.

Esto demuestra la importancia de utilizar rutas válidas para el usuario y no asumir que la raíz del disco del sistema es escribible.

---

# 14. Prueba de creación de procesos

Se utiliza directamente la API Win32:

```rust
CreateProcessW
```

La prueba crea un proceso hijo y espera su finalización.

El comando utilizado para el proceso hijo es:

```text
C:\Windows\System32\cmd.exe /C exit 7
```

Después de crear el proceso, se utilizan:

```rust
WaitForSingleObject
GetExitCodeProcess
CloseHandle
```

## 14.1. Qué valida

- Creación de procesos externos.
- Obtención de los handles del proceso y del thread.
- Espera hasta que el proceso finalice.
- Lectura del código de salida.
- Liberación de los handles.

Resultado observado:

```text
=== PROCESS TEST ===
[PASS] CreateProcessW
Child process exit code: 7
[PASS] Process wait + exit code validation
```

El código de salida `7` es intencional: permite verificar que la aplicación no solamente consigue crear el proceso, sino que también obtiene correctamente su código de finalización.

### Importancia

La creación de procesos puede ser útil para:

- Ejecutar herramientas externas.
- Separar tareas de procesamiento.
- Iniciar componentes auxiliares.
- Gestionar procesos independientes.

Esta prueba utiliza `CreateProcessW` y no `SHCreateProcessAsUserW`. La modificación de `SHCREATEPROCESSINFOW` se realizó para resolver una declaración faltante durante la compilación de `windows-sys`.

---

# 15. Resultado de las pruebas en hardware real

La suite se ejecutó directamente en una Microsoft Surface RT.

Resultado observado:

```text
====================================
 WINDOWS RT ARM32 - RUST TEST SUITE
====================================

=== TCP TEST ===
Server received: RT_ARM32_PING
[PASS] TCP client/server communication

=== THREADING TEST ===
Worker thread is running!
Worker returned: 42
[PASS] Rust thread creation and join

=== FILE API TEST ===

=== PROCESS TEST ===
[PASS] CreateProcessW
Child process exit code: 7
[PASS] Process wait + exit code validation

====================================
 RESULTS: 3/4 tests passed
====================================
```

La salida anterior corresponde a una ejecución anterior a la corrección de `GetTempPathW`.

Después de implementar la obtención dinámica del directorio temporal, el problema de archivos quedó resuelto.

Por lo tanto, el resultado actualizado es:

| Prueba | Resultado |
|---|---|
| TCP | PASS |
| Rust Threading | PASS |
| Win32 File API | PASS |
| Win32 Process API | PASS |

**Resultado final: 4/4 pruebas superadas.**

Esto confirma que las operaciones fundamentales probadas funcionan en Windows RT ARM32 con el ejecutable generado por el entorno de compilación preparado.

---

# 16. Problemas encontrados y soluciones

Esta sección registra los problemas más importantes encontrados durante el desarrollo.

## 16.1. Ejecutable no válido para la plataforma

Mensaje:

```text
The specified executable is not a valid application for this OS platform.
```

Se encontró durante las primeras pruebas con las herramientas de la toolchain.

La distinción importante es que las herramientas de compilación se ejecutan en el host Windows x64, mientras que el ejecutable generado está destinado a ARM32.

No debe confundirse la arquitectura del compilador/linker con la arquitectura del binario que generan.

## 16.2. Biblioteca windows.0.53.0.lib faltante

Durante el enlace aparecía una dependencia de:

```text
windows.0.53.0.lib
```

La solución utilizada fue:

```text
--cfg windows_raw_dylib
```

Esto permitió evitar la dependencia de esa biblioteca de importación en la configuración utilizada.

## 16.3. Tipos faltantes en windows-sys

Error representativo:

```text
cannot find type `SHCREATEPROCESSINFOW` in this scope
```

Causa:

La estructura estaba limitada a arquitecturas que no incluían ARM32.

Solución:

Se inspeccionó la definición y se agregó ARM32 a la variante de 32 bits, junto con su implementación `Default`.

Este mismo procedimiento se aplicó a otras estructuras que necesitaban estar disponibles en ARM32.

## 16.4. Features faltantes

Al utilizar funciones como `CreateFileW` y `CreateProcessW`, algunas declaraciones no estaban disponibles porque dependían de features adicionales.

Se revisaron los atributos `cfg(feature = "...")` y se agregaron las features correspondientes.

Entre ellas:

```toml
"Win32_Security",
"Win32_System_IO",
```

## 16.5. Diferencia de mutabilidad entre punteros

Error:

```text
expected raw pointer `*mut OVERLAPPED`
found raw pointer `*const _`
```

Causa:

Se utilizaba:

```rust
null()
```

Pero las funciones `ReadFile` y `WriteFile` esperan un puntero mutable para el parámetro `OVERLAPPED`.

Solución:

```rust
null_mut()
```

Diferencia:

| Función | Tipo |
|---|---|
| `null()` | `*const T` |
| `null_mut()` | `*mut T` |

No fue necesario modificar `windows-sys`; era un problema en el código de prueba.

## 16.6. Error de acceso al crear archivos

Errores observados:

```text
ERROR_ACCESS_DENIED (5)
ERROR_PATH_NOT_FOUND (3)
```

Causa:

La ruta inicial estaba en la raíz de `C:\` y después se utilizó una ruta que no correspondía a un directorio existente.

Solución:

Obtener la carpeta temporal mediante:

```rust
GetTempPathW
```

Esto permitió evitar rutas absolutas codificadas manualmente.

---

# 17. Inspección del ejecutable generado

También se verificó que el ejecutable generado correspondiera realmente a ARM32.

La inspección del binario confirmó:

```text
Format: COFF-ARM
Architecture: thumb
Bits: 32
Machine: IMAGE_FILE_MACHINE_ARMNT (0x1C4)
Subsystem: CUI
Subsystem version: 6.3
```

Estos datos son importantes porque permiten comprobar que el ejecutable no es simplemente un binario x86 o x64 que se intenta ejecutar en Windows RT.

La identificación `IMAGE_FILE_MACHINE_ARMNT` corresponde a la arquitectura ARM NT de 32 bits.

El subsistema `CUI` corresponde a una aplicación de consola.

---

# 18. Consideraciones importantes

## 18.1. Compatibilidad de estructuras

No todas las estructuras de Windows pueden reutilizar directamente sus definiciones x86 o x64.

Antes de agregar ARM32 a una condición `cfg`, hay que revisar:

- Tamaño de los punteros.
- Alineación.
- Uso de `repr(C)`.
- Uso de `repr(C, packed(1))`.
- Campos y tipos internos.
- Condiciones de compilación de los tipos anidados.

La compatibilidad de una estructura no se determina únicamente porque ARM32 y x86 tengan punteros de 32 bits.

## 18.2. Features de windows-sys

Una función puede existir en el código fuente y, aun así, no estar disponible durante la compilación.

Esto puede deberse a:

- Una feature deshabilitada.
- Una estructura asociada que no está disponible.
- Un `cfg` que excluye ARM32.
- Una dependencia de otro módulo.

Por eso es recomendable inspeccionar el código fuente cuando aparezcan errores de símbolos o tipos faltantes.

## 18.3. Código Win32 y Rust estándar

Las pruebas utilizan dos niveles distintos de abstracción:

**Rust estándar:**

```rust
std::net
std::thread
```

**APIs Win32 directas:**

```rust
CreateFileW
ReadFile
WriteFile
CreateProcessW
```

Ambos enfoques son válidos y complementarios.

La biblioteca estándar simplifica el desarrollo, mientras que Win32 permite acceder directamente a las capacidades del sistema operativo.

## 18.4. Resultado de las pruebas

Que las cuatro pruebas funcionen no significa que todas las funcionalidades de Rust estén soportadas en Windows RT.

Tampoco significa que cualquier crate de crates.io vaya a compilar o funcionar.

Cada dependencia puede utilizar:

- APIs que no existen en Windows RT.
- Funciones que no están disponibles para ARM32.
- Bibliotecas nativas adicionales.
- Features no habilitadas.
- Funcionalidades modernas de Windows que no existen en el sistema objetivo.

La compatibilidad debe verificarse progresivamente.

---

# 19. Próximos pasos

Con las pruebas fundamentales funcionando, el siguiente objetivo es ampliar la cobertura de APIs.

## 19.1. Threading Win32

Todavía falta probar directamente:

- `CreateThread`
- `WaitForSingleObject`
- `CloseHandle`
- `CreateMutexW`
- `ReleaseMutex`
- `CreateEventW`
- `SetEvent`
- `ResetEvent`

Esto permitirá comprobar las APIs de sincronización nativas de Windows, más allá de `std::thread`.

## 19.2. Sistema de archivos

Pruebas recomendadas:

- `GetTempPathW`
- `GetFileAttributesW`
- `CreateDirectoryW`
- `DeleteFileW`
- `MoveFileW`
- `CopyFileW`
- `FindFirstFileW`
- `FindNextFileW`

También conviene probar nombres de archivos y rutas Unicode.

## 19.3. Red

La prueba TCP actual es básica.

Próximas pruebas:

- TCP con transferencia de datos de mayor tamaño.
- UDP mediante WinSock.
- Comunicación entre dos dispositivos de la red local.
- Reconexión después de perder la conexión.
- Múltiples clientes.
- Transferencia sostenida de datos.
- Medición de latencia y throughput.

## 19.4. Memoria

Conviene comprobar:

- Asignación y liberación de memoria.
- Buffers grandes.
- Operaciones concurrentes sobre memoria compartida.
- Comportamiento ante presión de memoria.

Esto es especialmente importante en dispositivos con recursos limitados.

## 19.5. Multimedia y streaming

Para el objetivo final del proyecto, será necesario investigar y probar:

- APIs de audio disponibles en Windows RT.
- APIs de captura y procesamiento de video.
- Decodificación de video por software.
- Posibilidad de utilizar aceleración por hardware.
- Transporte de audio y video mediante TCP o UDP.
- Sincronización entre audio y video.

No se debe asumir que las APIs multimedia modernas de Windows están disponibles en Windows RT.

---

# 20. Estado actual del proyecto

| Área | Estado |
|---|---|
| Compilación Rust ARM32 | Funcionando |
| Toolchain Clang/LLD | Configurada |
| Windows SDK ARM32 | Configurado |
| Enlace de ejecutables | Funcionando |
| `windows-sys 0.60.2` personalizado | Implementado |
| Ejecución en Surface RT | Comprobada |
| TCP mediante `std::net` | Comprobado |
| Threads mediante `std::thread` | Comprobado |
| Creación y lectura de archivos Win32 | Comprobado |
| Creación de procesos Win32 | Comprobado |
| Threading Win32 directo | Pendiente |
| UDP / WinSock avanzado | Pendiente |
| APIs multimedia | Pendiente |
| Streaming de audio y video | Pendiente |

---

# 21. Conclusión

Se consiguió preparar un entorno de desarrollo capaz de generar ejecutables Rust para Windows RT ARM32 desde un host Windows x64.

La principal dificultad no fue únicamente generar código ARM32, sino adaptar las declaraciones de las APIs de Windows para que `windows-sys 0.60.2` pudiera compilar correctamente para esta arquitectura.

Mediante modificaciones específicas en las condiciones `cfg`, la habilitación de features y la configuración del linker y del SDK, se consiguió resolver los problemas de compilación y enlace.

Posteriormente, se desarrolló una suite de pruebas que permitió validar el funcionamiento del ejecutable directamente en una Microsoft Surface RT.

Las cuatro áreas fundamentales verificadas son:

- Comunicación TCP.
- Creación y sincronización de threads mediante Rust.
- Operaciones de archivos mediante Win32.
- Creación y gestión de procesos mediante Win32.

El resultado demuestra que es viable utilizar Rust para desarrollar aplicaciones Windows ARM32 en este entorno, siempre que se adapten las dependencias necesarias y se compruebe la compatibilidad de las APIs utilizadas.

Este entorno constituye la base para continuar investigando la viabilidad de una aplicación de streaming en Windows RT.

---

# Apéndice A. Comandos de compilación

Desde la raíz del proyecto:

```powershell
$flags = @(
    "--cfg windows_raw_dylib",
    "-C target-feature=+crt-static",
    "-C link-arg=/SUBSYSTEM:CONSOLE,6.03",
    "-C link-arg=/LIBPATH:E:\arm32-tools\winsdk\crt\lib\aarch",
    "-C link-arg=/LIBPATH:E:\arm32-tools\winsdk\sdk\lib\um\aarch",
    "-C link-arg=/LIBPATH:E:\arm32-tools\winsdk\sdk\lib\ucrt\aarch"
)

$env:RUSTFLAGS = $flags -join " "
```

Compilar:

```powershell
cargo build --target thumbv7a-pc-windows-msvc
```

Compilar en modo release:

```powershell
cargo build --release --target thumbv7a-pc-windows-msvc
```

Ejecutar en el host no es equivalente a ejecutar en ARM32. El ejecutable debe copiarse a la Surface RT para comprobar su funcionamiento real.

# Apéndice B. Comandos útiles de diagnóstico

Consultar el target instalado:

```powershell
rustup target list --installed
```

Consultar la versión de Rust:

```powershell
rustc --version --verbose
```

Consultar las features de windows-sys:

```powershell
cargo tree -e features -i windows-sys
```

Buscar una estructura en windows-sys:

```powershell
Select-String `
    -Path ".\vendor\windows-sys\src\Windows\Win32\UI\Shell\mod.rs" `
    -Pattern "SHCREATEPROCESSINFOW" `
    -Context 12,20
```

Consultar la carpeta temporal en Windows:

```powershell
$env:TEMP
```

Consultar rutas del usuario:

```powershell
$env:LOCALAPPDATA
$env:USERPROFILE
```

---

# Apéndice C. Mantenimiento de windows-sys personalizado

Se recomienda conservar las modificaciones realizadas en un repositorio Git.

También conviene mantener un registro de cada adaptación con:

- Archivo modificado.
- Nombre de la estructura.
- Arquitecturas originales.
- Arquitectura agregada.
- Motivo del cambio.
- Verificación de layout.
- Resultado de compilación.

Esto facilitará actualizar `windows-sys` en el futuro sin perder las modificaciones ni tener que repetir toda la investigación.

**Regla principal:** no modificar una declaración solamente para eliminar un error de compilación. Primero hay que determinar si la definición es ABI-compatible con ARM32.

---

**Fin del documento.**