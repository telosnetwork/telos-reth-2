#!/usr/bin/env bun
import { readdir, readFile, writeFile } from 'fs/promises'
import { join } from 'path'

const ASSETS_DIR = './docs/dist/public/assets'
const LINK_MARKER =
  /if\([\w$]+\)return\(0,([\w$]+)\.jsx\)\(`a`,\{\.\.\.([\w$]+),href:([\w$]+)\.to,rel:`noopener noreferrer`,target:`_blank`\}\);let\[[\w$]+,[\w$]+\]=\(\3\.to\|\|``\)\.split\(`#`\)/
const RUSTDOCS_NAVIGATION_GUARD =
  /if\(([\w$]+)\.to===`\/docs`\|\|\1\.to\?\.startsWith\(`\/docs\/`\)\)return\(0,[\w$]+\.jsx\)\(`a`,\{\.\.\.[\w$]+,href:\1\.to\}\)/

async function fixRustdocsNavigation() {
  const files = await readdir(ASSETS_DIR)
  let patchedFiles = 0

  for (const file of files) {
    if (!file.endsWith('.js')) continue

    const path = join(ASSETS_DIR, file)
    const code = await readFile(path, 'utf-8')

    if (RUSTDOCS_NAVIGATION_GUARD.test(code)) {
      patchedFiles++
      continue
    }

    const match = code.match(LINK_MARKER)
    if (!match) continue

    const [linkMarker, jsx, rest, props] = match
    const guard = `if(${props}.to===\`/docs\`||${props}.to?.startsWith(\`/docs/\`))return(0,${jsx}.jsx)(\`a\`,{...${rest},href:${props}.to});`

    await writeFile(path, code.replace(linkMarker, `${guard}${linkMarker}`))
    patchedFiles++
    console.log(`Patched Rustdocs navigation in ${path}`)
  }

  if (patchedFiles !== 1) {
    console.error(`Expected to patch one Vocs client navigation bundle, patched ${patchedFiles}`)
    process.exit(1)
  }
}

fixRustdocsNavigation().catch((error) => {
  console.error('Error fixing Rustdocs navigation:', error)
  process.exit(1)
})
