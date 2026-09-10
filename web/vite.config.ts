import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

/**
 * An entry point, as an absolute path.
 *
 * `new URL(...).pathname` would do were it not for the space in this
 * repository's directory name, which it percent-encodes and Rollup then
 * cannot open.
 */
const entry = (file: string) => fileURLToPath(new URL(file, import.meta.url));

/**
 * GitHub Pages serves this repository at `/<repo>/`, not at the root, so every
 * built URL needs that prefix. It is passed in rather than hard-coded because
 * the same build has to work from `python3 tools/serve.py` at `/`, and a base
 * baked in at compile time would break one of the two.
 *
 * `tools/build_web.sh` sets it; unset means the root, which is what local
 * development wants.
 */
const base = normalize(process.env.SITE_BASE);

/**
 * Exactly one leading and one trailing slash.
 *
 * The workflow passes GitHub's own `base_path`, which is `/<repo>` for a
 * project site and `/` for a user site. Appending a slash to the second gives
 * `//`, which browsers read as protocol-relative and resolve against a host
 * that does not exist. Normalizing here means the workflow can pass what
 * GitHub reports without knowing which kind of site this is.
 */
function normalize(value: string | undefined): string {
  const trimmed = (value ?? "").replace(/^\/+|\/+$/g, "");
  return trimmed === "" ? "/" : `/${trimmed}/`;
}

export default defineConfig({
  base,
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // The wasm module is already about a megabyte; a warning about it on every
    // build is noise, not information.
    chunkSizeWarningLimit: 2048,
    rollupOptions: {
      input: {
        main: entry("./index.html"),
        bench: entry("./bench.html"),
        // Pages serves this for every path it has no file for. Built rather
        // than dropped in public/ so it shares the stylesheet with the rest.
        notfound: entry("./404.html"),
      },
    },
  },
  // wasm-pack emits `new URL('qe_bg.wasm', import.meta.url)`, which Vite
  // rewrites to a hashed asset. Nothing else is needed to bundle it.
  server: { port: 8137, strictPort: true },
});
