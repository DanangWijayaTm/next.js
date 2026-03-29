#!/usr/bin/env node
//
// Caching wrapper for the Rust linker (rust-lld).
//
// sccache can't cache linking (proc-macros, cdylibs, bins). This wrapper
// sits in front of rust-lld and caches the linker output in the turbo
// remote cache, keyed by the output filename (which contains cargo's
// metadata hash encoding the Cargo.lock entry + features + profile).
//
// Set via RUSTFLAGS: -Clinker=/path/to/cached-linker
//
// Requires TURBO_API, TURBO_TOKEN, TURBO_TEAM env vars for cache access.
// Falls through to real rust-lld if cache is unavailable.

const { execFileSync, execSync } = require('child_process')
const { createHash } = require('crypto')
const path = require('path')
const fs = require('fs')

const args = process.argv.slice(2)

// Find the real linker from CACHED_LINKER_REAL env var.
// Set by the sccache action before overriding -Clinker=.
function findRealLinker() {
  if (process.env.CACHED_LINKER_REAL) {
    return process.env.CACHED_LINKER_REAL
  }
  // Fallback: platform default
  if (process.platform === 'linux') return 'cc'
  return 'rust-lld'
}

// Parse linker args to find output path
function parseArgs() {
  let outputPath = null
  const flags = []

  for (let i = 0; i < args.length; i++) {
    if (args[i] === '-o' && i + 1 < args.length) {
      outputPath = args[i + 1]
      i++ // skip the path
    } else {
      flags.push(args[i])
    }
  }

  return { outputPath, flags }
}

// Compute cache key from output filename + sorted flags
function computeCacheKey(outputPath, flags) {
  const outputName = path.basename(outputPath)
  const hash = createHash('sha256')
  hash.update('link-cache-v1\0')
  hash.update(outputName + '\0')
  // Sort flags for determinism (exclude -L paths which vary per machine)
  const stableFlags = flags.filter((f) => !f.startsWith('-L')).sort()
  hash.update(stableFlags.join('\0'))
  return hash.digest('hex')
}

async function main() {
  const { outputPath, flags } = parseArgs()

  // No output path or no token — just run the real linker
  if (!outputPath || !process.env.TURBO_TOKEN) {
    const linker = findRealLinker()
    execFileSync(linker, args, { stdio: 'inherit' })
    return
  }

  const key = computeCacheKey(outputPath, flags)

  let cache
  try {
    cache = await import('./turbo-cache.mjs')
  } catch {
    // turbo-cache not available — fall through to real linker
    const linker = findRealLinker()
    execFileSync(linker, args, { stdio: 'inherit' })
    return
  }

  // Check cache
  try {
    const data = await cache.get(key)
    if (data) {
      // Cache HIT — write cached binary to output path
      fs.mkdirSync(path.dirname(outputPath), { recursive: true })
      fs.writeFileSync(outputPath, data)
      fs.chmodSync(outputPath, 0o755)
      return
    }
  } catch {
    // Cache check failed — continue to real linker
  }

  // Cache MISS — run real linker
  const linker = findRealLinker()
  execFileSync(linker, args, { stdio: 'inherit' })

  // Cache the output (non-fatal)
  try {
    if (fs.existsSync(outputPath)) {
      await cache.put(key, outputPath)
    }
  } catch {
    // Upload failed — non-fatal
  }
}

main().catch(() => {
  // On any error, fall back to real linker
  try {
    const linker = findRealLinker()
    execFileSync(linker, args, { stdio: 'inherit' })
  } catch (e) {
    process.exit(e.status || 1)
  }
})
