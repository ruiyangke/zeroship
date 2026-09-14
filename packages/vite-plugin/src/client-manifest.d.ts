// Type shim for the `virtual:zeroship/client-manifest` virtual module
// the vite-plugin exposes to the SSR build. Import for side effects
// (e.g. via tsconfig "types" or a triple-slash reference) so user
// TypeScript code can `import` the module without "Cannot find module"
// errors.
//
// The shape mirrors Vite's own `Manifest` type
// (https://vite.dev/guide/build.html#manifest) — a plain string-keyed
// map of source-relative paths to chunk metadata.

declare module "virtual:zeroship/client-manifest" {
  export interface ManifestChunk {
    /** Hashed output filename relative to the build outDir. */
    file: string;
    /** Source path that produced this chunk. */
    src?: string;
    /** Whether the chunk is an entry point. */
    isEntry?: boolean;
    /** Whether the chunk is a dynamic import target. */
    isDynamicEntry?: boolean;
    /** CSS files emitted alongside the chunk. */
    css?: string[];
    /** Other assets the chunk imports (images, fonts). */
    assets?: string[];
    /** Static imports (other chunks). */
    imports?: string[];
    /** Dynamic imports (other chunks). */
    dynamicImports?: string[];
  }
  const manifest: Record<string, ManifestChunk>;
  export default manifest;
}
