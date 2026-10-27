// Inside the desktop app (Tauri), which runs the server on this computer.
export const isDesktop = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
