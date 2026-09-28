export function powershellLiteral(value) {
  if (/[\u0000-\u001f\u007f]/u.test(value)) throw new Error("Command arguments cannot contain control characters.");
  // PowerShell recognizes curly single quotes as delimiters as well as ASCII '.
  return `'${value.replace(/['\u2018\u2019\u201a\u201b]/gu, "$&$&")}'`;
}

export function suggestedOutput(path) {
  return `${path.split("/").filter(Boolean).at(-1) || "root"}-download`;
}

export function downloadCommand(profile, directory, outputName, pageUrl) {
  const stem = outputName.split(".")[0].toUpperCase();
  if (!outputName || /[\\/:<>"|?*\u0000-\u001f\u007f]/u.test(outputName)
      || /[. ]$/u.test(outputName) || [".", ".."].includes(outputName)
      || /^(CON|PRN|AUX|NUL|CLOCK\$|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3])$/u.test(stem)
      || outputName.toLowerCase().startsWith(".zfs-upload-")) {
    throw new Error("Enter a new output folder name, without path separators or reserved Windows names.");
  }
  const url = new URL("/api/download", pageUrl);
  url.searchParams.set("path", directory);
  const output = `${profile.download_folder.replace(/\\+$/u, "")}\\${outputName}`;
  return `& ${powershellLiteral(profile.client_path)} download ${powershellLiteral(url.href)} ${powershellLiteral(output)}`;
}
