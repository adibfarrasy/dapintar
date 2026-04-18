# dapintar

A Debug Adapter Protocol (DAP) server for JVM languages — Java, Groovy, and Kotlin — built to be lightweight and free.

## Why

Debugging JVM applications from a non-IntelliJ editor is harder than it should be. Existing DAP servers for Java either pull in a large toolchain, depend on a running JVM process, or only understand Java source. If you are working in a mixed-language JVM project — say, a Groovy build script alongside Java application code, or a Kotlin service with Java utilities — most open source debuggers cannot step into all of those source files.

dapintar speaks JDWP (the JVM's native debug wire protocol) directly and maps JVM class names back to source files for all three languages. It is a small, self-contained binary with no JVM dependency of its own.

**Status: alpha.** Launch and attach to JVM processes, set breakpoints, step, and inspect stack frames and variables. The debugger works for Java, Groovy, and Kotlin projects using Gradle.

## Features

- Launch JVM processes via Gradle and attach over JDWP
- Breakpoints — set before or after class load; adjusts to nearest executable line
- Step over, step into, step out
- Stack frame and variable inspection when paused
- Detects uncaught exceptions and out-of-memory errors; reports termination cause to the editor
- Source navigation into Java, Groovy, and Kotlin files in the same session

## Prerequisites

- Rust toolchain (`cargo`)
- `just` task runner (`cargo install just`)
- Java and Gradle available on `PATH` for the projects you debug

## Building from source

```bash
git clone https://github.com/adibfarrasy/dapintar
cd dapintar
just b
```

The binary is at `target/release/dapintar`.

## Installation

### Neovim

Use `nvim-dap` to register dapintar as a Java/Groovy/Kotlin adapter:

```lua
local dap = require('dap')

local adapter = {
  type = 'executable',
  command = '/path/to/dapintar',
}

dap.adapters.java = adapter
dap.adapters.groovy = adapter
dap.adapters.kotlin = adapter

dap.configurations.java = {
  {
    type = 'java',
    request = 'launch',
    name = 'Launch (Gradle)',
    projectDir = '${workspaceFolder}',
  },
}
-- Mirror the same configuration for groovy and kotlin if needed.
```

Replace `/path/to/dapintar` with the path to the binary you built.

### VS Code and Cursor

1. Install the [DAP extension](https://marketplace.visualstudio.com/items?itemName=ms-python.debugpy) or any extension that supports custom debug adapters (the built-in debug UI works).

2. Add a launch configuration in `.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Debug with dapintar",
      "type": "dapintar",
      "request": "launch",
      "projectDir": "${workspaceFolder}"
    }
  ]
}
```

3. Register dapintar as the adapter for the `dapintar` type. In your VS Code settings or a workspace extension, point the `debugAdapterExecutable` at the binary:

```json
{
  "dapintar.adapterPath": "/path/to/dapintar"
}
```

> Note: a first-class VS Code extension for dapintar is planned. For now, wiring it up requires the steps above.

## Development

```bash
# Build
just b

# Run all tests (includes integration tests)
just t

# Run a specific test by name substring
just t test_name_substring
```

Integration tests launch real Gradle projects and attach over JDWP. They require Java and Gradle on `PATH`.

## License

MIT
