export async function resolve(specifier, context, nextResolve) {
  try {
    return await nextResolve(specifier, context);
  } catch (error) {
    if (
      (error?.code === "ERR_MODULE_NOT_FOUND" || error?.code === "ERR_UNSUPPORTED_DIR_IMPORT")
      && (specifier.startsWith("./") || specifier.startsWith("../"))
      && !specifier.match(/\.[cm]?[jt]sx?$/)
    ) {
      // Try the extensionless and directory-import forms Node's ESM resolver
      // won't guess: ./foo -> ./foo.ts, ./dir -> ./dir.ts / ./dir/index.ts.
      for (const candidate of [`${specifier}.ts`, `${specifier}/index.ts`]) {
        try {
          return await nextResolve(candidate, context);
        } catch {}
      }
    }
    throw error;
  }
}
