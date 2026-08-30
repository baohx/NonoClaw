// Apply the persisted theme before the application mounts to avoid a light flash.
(() => {
  let theme = "frost";
  try {
    const saved = localStorage.getItem("nonoclaw:theme");
    if (saved) theme = saved;
  } catch {
    // Storage may be unavailable in hardened/private browser contexts.
  }
  const darkThemes = new Set(["frost", "indigo", "burgundy", "espresso", "navy"]);
  document.documentElement.setAttribute("data-theme", theme);
  document.documentElement.setAttribute(
    "data-color-scheme",
    darkThemes.has(theme) ? "dark" : "light",
  );
})();
