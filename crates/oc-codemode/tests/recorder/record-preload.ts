// Test preload for recording (record.sh passes it with --preload). Wraps bun:test's `describe` and `test` so the
// recorder knows the running test: describe titles and the test title joined with " > ", `test.each` titles
// formatted. Does nothing unless CODEMODE_RECORD_DIR is set.
import * as real from "bun:test"
import * as Recorder from "./src/codemode/record.js"

const fileOfCaller = (): string | undefined => {
  const stack = new Error().stack ?? ""
  for (const line of stack.split("\n")) {
    const match = line.match(/([^/\\\s(]+\.test\.ts):\d+/)
    if (match) return match[1]
  }
  return undefined
}

// The originals, captured before mock.module replaces the namespace's bindings.
const realDescribe = real.describe as unknown as Fn
const realTest = real.test as unknown as Fn
const realExports = { ...real }

let prefix: ReadonlyArray<string> = []

type Fn = (...args: Array<any>) => any

// bun's modifiers are not all own properties, so name them as well.
const modifiers = ["skip", "only", "todo", "if", "skipIf", "todoIf", "failing", "concurrent", "serial", "each"]

const copyProperties = (target: Fn, source: Fn, skip: ReadonlyArray<string>) => {
  for (const key of new Set([...Object.getOwnPropertyNames(source), ...modifiers])) {
    if (!(key in source)) continue
    if (key === "length" || key === "name" || key === "prototype" || skip.includes(key)) continue
    try {
      ;(target as any)[key] = (source as any)[key]
    } catch {}
  }
}

const group = (define: Fn, name: unknown, fn: unknown, rest: Array<unknown>) => {
  const mine = [...prefix, String(name)]
  if (typeof fn !== "function") return define(name, fn, ...rest)
  return define(
    name,
    (...args: Array<unknown>) => {
      const saved = prefix
      prefix = mine
      try {
        return (fn as Fn)(...args)
      } finally {
        prefix = saved
      }
    },
    ...rest,
  )
}
const describe: Fn = (name: unknown, fn?: unknown, ...rest: Array<unknown>) => group(realDescribe, name, fn, rest)
copyProperties(describe, realDescribe, [])

const settleTest = (context: ReturnType<typeof Recorder.testStart>, result: unknown) => {
  if (result !== null && typeof result === "object" && typeof (result as PromiseLike<unknown>).then === "function") {
    return (result as Promise<unknown>).then(
      (value) => {
        Recorder.testEnd(context)
        return value
      },
      (error) => {
        Recorder.testEnd(context)
        throw error
      },
    )
  }
  Recorder.testEnd(context)
  return result
}

const register = (name: string, fn: unknown, rest: Array<unknown>, define: Fn = realTest) => {
  const file = fileOfCaller() ?? "unknown"
  Recorder.registerFile(file)
  const full = [...prefix, name].join(" > ")
  if (typeof fn !== "function") return define(name, fn, ...rest)
  return define(
    name,
    () => {
      const context = Recorder.testStart(file, full)
      let result: unknown
      try {
        result = (fn as Fn)()
      } catch (error) {
        Recorder.testEnd(context)
        throw error
      }
      return settleTest(context, result)
    },
    ...rest,
  )
}

const pretty = (value: unknown): string => {
  if (typeof value === "string") return JSON.stringify(value)
  return Bun.inspect(value)
}

// Placeholder formatting as bun's test.each does it: each placeholder consumes the next argument; `%#` is the row index.
const formatTitle = (title: string, args: ReadonlyArray<unknown>, index: number, row: unknown): string => {
  if (row !== null && typeof row === "object" && !Array.isArray(row) && title.includes("$")) {
    return title.replace(/\$([A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*)/g, (whole, path: string) => {
      let value: any = row
      for (const key of path.split(".")) {
        if (value === null || value === undefined || !(key in Object(value))) return whole
        value = value[key]
      }
      return typeof value === "string" ? value : pretty(value)
    })
  }
  let next = 0
  return title.replace(/%[sdifjop#%]/g, (token) => {
    if (token === "%#") return String(index)
    if (token === "%%") return token
    if (next >= args.length) return token
    const value = args[next++]
    switch (token) {
      case "%s":
        return typeof value === "string" ? value : pretty(value)
      case "%j":
        return JSON.stringify(value) ?? ""
      case "%d":
      case "%i":
        return String(token === "%i" ? Math.trunc(Number(value)) : Number(value))
      case "%f":
        return String(Number(value))
      default:
        return pretty(value)
    }
  })
}

const test: Fn = (name: unknown, fn?: unknown, ...rest: Array<unknown>) => register(String(name), fn, rest)
copyProperties(test, realTest, ["each"])
// bun's modifiers check their receiver, so each calls the real one on bun's own `test` (and `describe`) and keeps
// the recording wrapper around the body.
const realModifier = (owner: Fn, key: string): Fn | undefined => {
  try {
    const modifier = (owner as any)[key]
    return typeof modifier === "function" ? (modifier as Fn).bind(owner) : undefined
  } catch {
    // Some modifiers are getters that throw where they don't apply (`describe.failing`).
    return undefined
  }
}
for (const key of ["skip", "only", "todo", "failing", "concurrent", "serial"]) {
  const real = realModifier(realTest, key)
  if (real !== undefined)
    (test as any)[key] = (name: unknown, fn?: unknown, ...rest: Array<unknown>) =>
      register(String(name), fn, rest, real)
  const realGroup = realModifier(realDescribe, key)
  if (realGroup !== undefined)
    (describe as any)[key] = (name: unknown, fn?: unknown, ...rest: Array<unknown>) =>
      group(realGroup, name, fn, rest)
}
for (const key of ["if", "skipIf", "todoIf"]) {
  const real = realModifier(realTest, key)
  if (real !== undefined)
    (test as any)[key] =
      (condition: unknown) =>
      (name: unknown, fn?: unknown, ...rest: Array<unknown>) =>
        register(String(name), fn, rest, real(condition))
  const realGroup = realModifier(realDescribe, key)
  if (realGroup !== undefined)
    (describe as any)[key] =
      (condition: unknown) =>
      (name: unknown, fn?: unknown, ...rest: Array<unknown>) =>
        group(realGroup(condition), name, fn, rest)
}
;(test as any).each =
  (table: ReadonlyArray<unknown>) =>
  (title: string, fn: Fn, ...rest: Array<unknown>) => {
    table.forEach((row, index) => {
      const args = Array.isArray(row) ? row : [row]
      register(formatTitle(title, args, index, row), () => fn(...args), rest)
    })
  }

if (Recorder.enabled) {
  realExports.mock.module("bun:test", () => ({ ...realExports, describe, test, it: test }))
}
