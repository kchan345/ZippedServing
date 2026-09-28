"use strict";

const $ = (id) => document.getElementById(id);
let currentPath = "";
let maxUploadBytes = 0;
let uploading = false;
let activeUpload = null;
let cancelRequested = false;
let listingRequest = 0;

const childPath = (parent, name) => parent ? `${parent}/${name}` : name;
const apiUrl = (action, path, extra = {}) =>
  `/api/${action}?${new URLSearchParams({ path, ...extra })}`;
const directoryUrl = (path) => `/?${new URLSearchParams({ path })}`;
function message(text, error = false) {
  $("status").textContent = text;
  $("status").className = error ? "error" : "";
}
function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value.toFixed(1)} ${units[unit]}`;
}
async function checked(response) {
  if (!response.ok) {
    const text = await response.text();
    let detail = text;
    try { detail = JSON.parse(text).error || text; } catch { /* Plain-text framework error. */ }
    throw new Error(detail || `HTTP ${response.status}`);
  }
  return response;
}
function directoryLink(label, path) {
  const link = document.createElement("a");
  link.textContent = label;
  link.href = directoryUrl(path);
  link.addEventListener("click", (event) => {
    if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || event.button !== 0) return;
    event.preventDefault();
    loadDirectory(path).catch((error) => message(error.message, true));
  });
  return link;
}
async function loadDirectory(path, push = true) {
  const request = ++listingRequest;
  const response = await checked(await fetch(apiUrl("list", path)));
  const listing = await response.json();
  if (request !== listingRequest) return;
  currentPath = listing.path;
  maxUploadBytes = listing.max_upload_bytes;
  if (push) history.pushState(null, "", directoryUrl(currentPath));
  $("breadcrumbs").replaceChildren(directoryLink("Root", ""));
  let accumulated = "";
  for (const part of currentPath.split("/").filter(Boolean)) {
    accumulated = childPath(accumulated, part);
    $("breadcrumbs").append(" / ", directoryLink(part, accumulated));
  }
  $("download-directory").href = apiUrl("download", currentPath);
  $("limit").textContent = `Upload limit: ${size(maxUploadBytes)} per file. Files upload sequentially.`;
  $("entries").replaceChildren();
  for (const entry of listing.entries) {
    const path = childPath(currentPath, entry.name);
    const row = document.createElement("tr");
    const cells = Array.from({ length: 4 }, () => document.createElement("td"));
    if (entry.kind === "directory") {
      cells[0].append(directoryLink(`${entry.name}/`, path));
    } else {
      cells[0].textContent = entry.name;
    }
    cells[1].textContent = entry.kind === "file" ? size(entry.size) : "-";
    cells[2].textContent = entry.modified == null ? "-" : new Date(entry.modified * 1000).toLocaleString();
    if (entry.kind === "blocked") {
      cells[3].textContent = "Blocked";
    } else {
      const compressed = document.createElement("a");
      compressed.href = apiUrl("download", path);
      compressed.textContent = entry.kind === "directory" ? ".tar.lz4" : ".lz4";
      cells[3].append(compressed);
      if (entry.kind === "file") {
        const raw = document.createElement("a");
        raw.href = apiUrl("download", path, { format: "raw" });
        raw.textContent = "Raw";
        cells[3].append(" | ", raw);
      }
    }
    row.append(...cells);
    $("entries").append(row);
  }
  if (!listing.entries.length) message("This directory is empty.");
  else if (!uploading) message(`${listing.entries.length} entries`);
}
function uploadFile(file, path, index, total) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    activeUpload = xhr;
    xhr.open("PUT", apiUrl("upload", childPath(path, file.name)));
    xhr.setRequestHeader("X-Requested-With", "zipped-file-serving");
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    $("progress-label").textContent = `${index}/${total}: ${file.name}`;
    $("progress").value = 0;
    xhr.upload.onprogress = (event) => {
      if (event.lengthComputable) $("progress").value = event.loaded / event.total * 100;
      if (event.lengthComputable && event.loaded === event.total) {
        $("progress-label").textContent = `${index}/${total}: ${file.name} - saving on server...`;
      }
    };
    xhr.onload = () => {
      if (xhr.status === 201) resolve();
      else {
        let detail = xhr.responseText;
        try { detail = JSON.parse(detail).error || detail; } catch { /* Plain-text framework error. */ }
        reject(new Error(`${file.name}: ${detail || `HTTP ${xhr.status}`}`));
      }
    };
    xhr.onerror = () => reject(new Error(`${file.name}: network error; upload may not have completed`));
    xhr.onabort = () => reject(new Error("Upload cancelled. Files already completed are retained."));
    xhr.send(file);
  });
}
async function uploadFiles(files) {
  if (uploading || !files.length) return;
  uploading = true;
  cancelRequested = false;
  $("upload").disabled = true;
  $("transfer").hidden = false;
  const destination = currentPath;
  let completed = 0;
  let failure = null;
  try {
    for (const file of files) {
      if (cancelRequested) throw new Error("Upload cancelled.");
      if (file.size > maxUploadBytes) throw new Error(`${file.name} exceeds the upload size limit.`);
      await uploadFile(file, destination, completed + 1, files.length);
      completed++;
    }
  } catch (error) {
    failure = error.message;
  } finally {
    activeUpload = null;
    uploading = false;
    $("upload").disabled = false;
    $("upload").value = "";
    $("transfer").hidden = true;
  }
  try { await loadDirectory(currentPath, false); }
  catch (error) { failure = `${failure ? `${failure} ` : ""}Refresh failed: ${error.message}`; }
  message(`${completed} file(s) uploaded.${failure ? ` ${failure}` : ""}`, Boolean(failure));
}
$("upload").addEventListener("change", (event) => uploadFiles(Array.from(event.target.files)));
$("cancel-upload").addEventListener("click", () => {
  cancelRequested = true;
  if (activeUpload) activeUpload.abort();
});
$("refresh").addEventListener("click", () => loadDirectory(currentPath, false).catch((error) => message(error.message, true)));
$("mkdir").addEventListener("click", async () => {
  const name = prompt("New folder name");
  if (name === null) return;
  if (!name || /[/\\]/.test(name)) { message("Enter one folder name, without path separators.", true); return; }
  try {
    await checked(await fetch(apiUrl("mkdir", childPath(currentPath, name)), {
      method: "POST", headers: { "X-Requested-With": "zipped-file-serving" }
    }));
    await loadDirectory(currentPath, false);
    message(`Created ${name}`);
  } catch (error) { message(error.message, true); }
});
for (const eventName of ["dragover", "drop"]) {
  document.addEventListener(eventName, (event) => event.preventDefault());
}
$("drop-zone").addEventListener("drop", (event) => {
  const items = Array.from(event.dataTransfer.items || []);
  if (items.some((item) => item.webkitGetAsEntry?.()?.isDirectory)) {
    message("Folder upload is not supported. Create a folder and upload its files instead.", true);
    return;
  }
  uploadFiles(Array.from(event.dataTransfer.files));
});
window.addEventListener("popstate", () => {
  loadDirectory(new URLSearchParams(location.search).get("path") || "", false)
    .catch((error) => message(error.message, true));
});
loadDirectory(new URLSearchParams(location.search).get("path") || "", false)
  .catch((error) => message(error.message, true));
