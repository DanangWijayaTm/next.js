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
const os = require('os')

const args = process.argv.slice(2)

// Log file for diagnostics (printed by sccache stop step)
const LOG_FILE = path.join(
  process.env.RUNNER_TEMP || os.tmpdir(),
  'cached-linker.log'
)

function log(msg) {
  fs.appendFileSync(LOG_FILE, `${new Date().toISOString()} ${msg}\n`)
}

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

// Extract a human-readable crate name from the output path.
// Build scripts: target/release/build/serde-HASH/build_script_build-HASH → "serde"
// Deps: target/release/deps/libserde_derive-HASH.so → "serde_derive"
function extractCrateName(outputPath) {
  const parts = outputPath.split(path.sep)
  const buildIdx = parts.indexOf('build')
  if (buildIdx >= 0 && buildIdx + 1 < parts.length) {
    // Build script: parent dir is "crate-HASH"
    return parts[buildIdx + 1].replace(/-[a-f0-9]+$/, '')
  }
  // Deps: filename is "libfoo-HASH.ext" or "foo-HASH.ext"
  const base = path.basename(outputPath)
  return base.replace(/^lib/, '').replace(/-[a-f0-9]+\..*$/, '')
}

// Compute cache key from output filename only.
// The output filename (e.g., build_script_build-90d8775c9a66df81) contains
// cargo's metadata hash which encodes the crate version, features, profile,
// and all dependency metadata. This is sufficient as a cache key.
// We do NOT hash linker flags because they contain temp paths (rustcXXXXXX)
// that change every build.
function computeCacheKey(outputPath) {
  const outputName = path.basename(outputPath)
  const hash = createHash('sha256')
  hash.update('link-cache-v1\0')
  hash.update(outputName + '\0')
  return hash.digest('hex')
}

async function main() {
  const { outputPath, flags } = parseArgs()
  const crateName = outputPath ? extractCrateName(outputPath) : '(unknown)'
  const outputName = outputPath ? path.basename(outputPath) : '(unknown)'

  // Log the full linker invocation for diagnostics
  log(`INVOKE [${crateName}] -o ${outputPath}`)
  log(`  args: ${args.join(' ')}`)

  // No output path or no token — just run the real linker
  if (!outputPath || !process.env.TURBO_TOKEN) {
    const linker = findRealLinker()
    log(`  PASSTHROUGH (${!outputPath ? 'no -o' : 'no token'})`)
    execFileSync(linker, args, { stdio: 'inherit' })
    return
  }

  const key = computeCacheKey(outputPath)

  let cache
  try {
    cache = await import('./turbo-cache.mjs')
  } catch (e) {
    // turbo-cache not available — fall through to real linker
    log(`  PASSTHROUGH (turbo-cache import failed: ${e.message})`)
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
      log(`  HIT (${data.length} bytes, key ${key.slice(0, 16)})`)
      return
    }
  } catch (e) {
    log(`  CACHE_ERROR get: ${e.message}`)
    // Cache check failed — continue to real linker
  }

  // Cache MISS — run real linker
  const linker = findRealLinker()
  log(`  MISS (linker=${linker}, key ${key.slice(0, 16)})`)
  execFileSync(linker, args, { stdio: 'inherit' })

  // Cache the output (non-fatal)
  try {
    if (fs.existsSync(outputPath)) {
      const size = fs.statSync(outputPath).size
      await cache.put(key, outputPath)
      log(`  STORED (${size} bytes)`)
    }
  } catch (e) {
    log(`  STORE_ERROR: ${e.message}`)
  }
}

main().catch((e) => {
  log(`  ERROR: ${e.message}`)
  // On any error, fall back to real linker
  try {
    const linker = findRealLinker()
    execFileSync(linker, args, { stdio: 'inherit' })
  } catch (e2) {
    process.exit(e2.status || 1)
  }
})
