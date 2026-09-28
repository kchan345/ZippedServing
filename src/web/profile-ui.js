import { downloadCommand, suggestedOutput } from "./commands.js";

export function initializeProfiles(checked) {
  const $ = (id) => document.getElementById(id);
  let profiles = [];
  let revision = 0;
  let ready = false;
  let busy = false;
  let dirty = false;
  let directory = "";

  function profileMessage(text, error = false) {
    $("profile-status").textContent = text;
    $("profile-status").className = error ? "error" : "";
  }
  function commandMessage(text, error = false) {
    $("command-status").textContent = text;
    $("command-status").className = error ? "error" : "";
  }
  function controls() {
    const disabled = !ready || busy;
    $("profile-select").disabled = disabled;
    $("profile-new").disabled = disabled || profiles.length >= 99;
    $("profile-delete").disabled = disabled || !$("profile-select").value;
    $("profile-reload").disabled = busy;
    $("profile-fields").disabled = disabled || (!$("profile-select").value && profiles.length >= 99);
    $("profile-count").textContent = `${profiles.length} / 99 profiles`;
  }
  function renderCommand() {
    $("command-text").value = "";
    $("command-copy").disabled = true;
    if (!ready || busy) return;
    const profile = profiles.find((item) => item.id === $("profile-select").value);
    if (!profile || dirty) {
      commandMessage("Save or choose a profile to generate its command.");
      return;
    }
    try {
      $("command-text").value = downloadCommand(profile, directory, $("command-output").value, location.href);
      $("command-copy").disabled = false;
      commandMessage("Paste into PowerShell on the client machine. This page does not run commands.");
    } catch (error) { commandMessage(error.message, true); }
  }
  function selectCommandDirectory(path) {
    directory = path;
    $("command-directory").textContent = path || "Root";
    $("command-output").value = suggestedOutput(path);
    renderCommand();
  }
  function showCommand(path) {
    selectCommandDirectory(path);
    $("client-command").open = true;
    $("client-command").scrollIntoView({ behavior: "smooth", block: "start" });
  }
  function fillProfile() {
    const profile = profiles.find((item) => item.id === $("profile-select").value);
    $("profile-name").value = profile?.name || "";
    $("profile-client").value = profile?.client_path || "";
    $("profile-folder").value = profile?.download_folder || "";
    dirty = false;
    controls();
    renderCommand();
  }
  function accept(store, selectedId) {
    profiles = store.profiles;
    revision = store.revision;
    ready = true;
    const select = $("profile-select");
    select.replaceChildren();
    if (profiles.length < 99) {
      const option = document.createElement("option");
      option.value = "";
      option.textContent = "New profile...";
      select.append(option);
    }
    for (const profile of profiles) {
      const option = document.createElement("option");
      option.value = profile.id;
      option.textContent = profile.name;
      select.append(option);
    }
    select.value = profiles.some((item) => item.id === selectedId) ? selectedId : profiles[0]?.id || "";
    fillProfile();
  }
  async function load() {
    if (busy) return;
    busy = true;
    controls();
    renderCommand();
    try {
      const store = await (await checked(await fetch("/api/profiles"))).json();
      accept(store, $("profile-select").value);
      profileMessage("Profiles loaded from the server. Choose one or save a new profile.");
    } catch (error) {
      ready = false;
      profileMessage(`Cannot load profiles: ${error.message}`, true);
    } finally {
      busy = false;
      controls();
      renderCommand();
    }
  }
  $("profile-select").addEventListener("change", fillProfile);
  $("profile-new").addEventListener("click", () => {
    $("profile-select").value = "";
    fillProfile();
    $("profile-name").focus();
  });
  $("profile-reload").addEventListener("click", () => {
    if (dirty && !confirm("Discard unsaved profile edits and reload?")) return;
    load();
  });
  for (const id of ["profile-name", "profile-client", "profile-folder"]) {
    $(id).addEventListener("input", () => { dirty = true; renderCommand(); });
  }
  $("profile-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (busy || !ready) return;
    const id = $("profile-select").value;
    const previousIds = new Set(profiles.map((item) => item.id));
    busy = true;
    controls();
    renderCommand();
    try {
      const response = await checked(await fetch(id ? `/api/profiles/${encodeURIComponent(id)}` : "/api/profiles", {
        method: id ? "PUT" : "POST",
        headers: { "Content-Type": "application/json", "X-Requested-With": "zipped-file-serving" },
        body: JSON.stringify({
          revision,
          name: $("profile-name").value,
          client_path: $("profile-client").value,
          download_folder: $("profile-folder").value
        })
      }));
      const store = await response.json();
      accept(store, id || store.profiles.find((item) => !previousIds.has(item.id))?.id);
      profileMessage("Profile saved on the server.");
    } catch (error) { profileMessage(`Profile not saved: ${error.message}`, true); }
    finally { busy = false; controls(); renderCommand(); }
  });
  $("profile-delete").addEventListener("click", async () => {
    const id = $("profile-select").value;
    if (busy || !id || !confirm("Delete this shared profile for everyone?")) return;
    busy = true;
    controls();
    renderCommand();
    try {
      const response = await checked(await fetch(`/api/profiles/${encodeURIComponent(id)}?revision=${revision}`, {
        method: "DELETE", headers: { "X-Requested-With": "zipped-file-serving" }
      }));
      accept(await response.json(), "");
      profileMessage("Profile deleted from the server.");
    } catch (error) { profileMessage(`Profile not deleted: ${error.message}`, true); }
    finally { busy = false; controls(); renderCommand(); }
  });
  $("command-output").addEventListener("input", renderCommand);
  $("command-copy").addEventListener("click", async () => {
    const text = $("command-text").value;
    if (!text) return;
    try {
      if (navigator.clipboard?.writeText && window.isSecureContext) {
        await navigator.clipboard.writeText(text);
      } else {
        $("command-text").focus();
        $("command-text").select();
        if (!document.execCommand("copy")) throw new Error("Clipboard access is unavailable");
      }
      commandMessage("Command copied.");
    } catch (error) {
      $("command-text").focus();
      $("command-text").select();
      commandMessage(`Copy failed: ${error.message}. The command is selected; press Ctrl+C to copy it.`, true);
    }
  });
  load();
  return { selectCommandDirectory, showCommand };
}
