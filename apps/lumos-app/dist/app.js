const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const shellEl = document.getElementById("shell");
const queryEl = document.getElementById("query");
const resultsEl = document.getElementById("results");
const addProjectBtn = document.getElementById("add-project-btn");
const dragVeilEl = document.getElementById("drag-veil");
const searchIconEl = document.getElementById("search-icon");
const clearBtn = document.getElementById("clear-btn");
const scanBarEl = document.getElementById("scan-status-bar");
const scanBarTextEl = document.getElementById("scan-status-text");

let results = [];
let selected = 0;
let debounceTimer = null;
let currentQuery = "";
let scanBarHideTimer = null;

// "search" | "add-project" — gates which keyboard/click handlers are live,
// so opening the add-project panel can't accidentally "open" a stale search
// result via Enter, and Escape backs out of the panel instead of closing
// the whole overlay.
let mode = "search";
let pendingProjectPath = null;

const EXT_STYLE = {
  js: ["JS", "#e6b400"], jsx: ["JSX", "#e6b400"], ts: ["TS", "#3178c6"], tsx: ["TSX", "#3178c6"],
  vue: ["V", "#41b883"], json: ["{ }", "#8bc34a"], md: ["MD", "#8b8b93"],
  yml: ["YML", "#a970ff"], yaml: ["YML", "#a970ff"], css: ["CSS", "#5b9dd9"], scss: ["SCSS", "#cc6699"],
  html: ["</>", "#e8734a"], rs: ["RS", "#b7743a"], toml: ["TOML", "#a970ff"],
  cs: ["C#", "#8c5ad1"], csproj: ["C#", "#8c5ad1"], sln: ["SLN", "#8c5ad1"],
  php: ["PHP", "#6f7cba"], py: ["PY", "#3776ab"], ini: ["INI", "#8b8b93"], config: ["CFG", "#8b8b93"],
  txt: ["TXT", "#6b6b73"], sql: ["SQL", "#4a90a4"],
};

const CLIP_STYLE = {
  json: ["{ }", "#8bc34a"], sql: ["SQL", "#4a90a4"], jwt: ["JWT", "#c46dd0"],
  uuid: ["ID", "#5a5a63"], stacktrace: ["ERR", "#e8734a"], url: ["URL", "#5b9dd9"],
  text: ["TXT", "#6b6b73"],
};

function iconFor(ext) {
  const key = (ext || "").toLowerCase();
  return EXT_STYLE[key] || [(key || "?").slice(0, 4).toUpperCase(), "#5a5a63"];
}

function clipIconFor(detectedType) {
  return CLIP_STYLE[detectedType] || CLIP_STYLE.text;
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatRelativeTime(unixSeconds) {
  const diff = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

function clipboardPreview(text) {
  return text.replace(/\s+/g, " ").trim();
}

function escapeHtml(s) {
  return (s || "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function escapeRegExp(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// Wraps whichever query words appear in `text` with `.match` — built on the
// already-escaped string so it can't reopen an HTML-escaping hole through
// the query itself.
function highlightText(text, query) {
  const escaped = escapeHtml(text);
  const terms = query
    .trim()
    .split(/\s+/)
    .filter(Boolean)
    .map((t) => escapeRegExp(escapeHtml(t)));
  if (!terms.length) return escaped;
  const pattern = new RegExp(`(${terms.join("|")})`, "ig");
  return escaped.replace(pattern, '<span class="match">$1</span>');
}

function updateClearButton() {
  clearBtn.classList.toggle("visible", queryEl.value.length > 0);
}

// Rebuilds the DOM — only when the result *set* changes (new search, or the
// overlay was just reopened). Moving the selection must never call this: a
// full innerHTML replace re-triggers every row's entrance animation at once,
// which is what "flicker" actually was.
function renderResults() {
  if (results.length === 0) {
    const hasQuery = queryEl.value.trim().length > 0;
    resultsEl.innerHTML = `
      <div id="empty-state">
        <div class="big">${hasQuery ? "∅" : "◔"}</div>
        <div>${hasQuery ? "No matches" : "Start typing to search your files"}</div>
      </div>`;
    return;
  }

  resultsEl.innerHTML = results
    .map((r, i) => {
      if (r.kind === "clipboard") {
        const [label, color] = clipIconFor(r.detected_type);
        return `
          <div class="result ${i === selected ? "selected" : ""}" data-index="${i}">
            <div class="file-icon" style="background:${color}">${label}</div>
            <div class="result-text">
              <div class="result-name">${highlightText(clipboardPreview(r.text_content), currentQuery)}</div>
              <div class="result-path">Clipboard · ${formatRelativeTime(r.created_at)}</div>
            </div>
            <div class="result-meta">
              <span class="badge">${escapeHtml(r.detected_type)}</span>
            </div>
          </div>`;
      }

      const [label, color] = iconFor(r.extension);
      const project = r.project_name ? `<span class="badge">${escapeHtml(r.project_name)}</span>` : "";
      return `
        <div class="result ${i === selected ? "selected" : ""}" data-index="${i}">
          <div class="file-icon" style="background:${color}">${label}</div>
          <div class="result-text">
            <div class="result-name">${highlightText(r.filename, currentQuery)}</div>
            <div class="result-path">${highlightText(r.path, currentQuery)}</div>
          </div>
          <div class="result-meta">
            ${project}
            <span class="size">${formatSize(r.size)}</span>
          </div>
        </div>`;
    })
    .join("");

  scrollSelectedIntoView();
}

// Just moves the highlight — toggles a class on existing nodes so the CSS
// transition can animate it smoothly instead of the whole list flashing in.
function updateSelection() {
  resultsEl.querySelectorAll(".result").forEach((el) => {
    el.classList.toggle("selected", Number(el.dataset.index) === selected);
  });
  scrollSelectedIntoView();
}

function scrollSelectedIntoView() {
  const selectedEl = resultsEl.querySelector(".result.selected");
  if (selectedEl) selectedEl.scrollIntoView({ block: "nearest" });
}

async function runSearch(query) {
  currentQuery = query;
  if (!query.trim()) {
    results = [];
    selected = 0;
    renderResults();
    return;
  }
  searchIconEl.classList.add("searching");
  try {
    results = await invoke("search_files", { query });
    selected = 0;
    renderResults();
  } catch (e) {
    console.error("search failed", e);
  } finally {
    searchIconEl.classList.remove("searching");
  }
}

function clearSearch() {
  queryEl.value = "";
  currentQuery = "";
  results = [];
  selected = 0;
  updateClearButton();
  renderResults();
  queryEl.focus();
}

// The Rust side handles whether the overlay hides or stays put — it drops
// always-on-top and waits as long as it takes to see if focus bounces back
// (dialog cancelled) or moves on for good (file opened) before deciding.
function activate(r, action) {
  if (!r) return;
  if (r.kind === "clipboard") {
    invoke("copy_to_clipboard", { text: r.text_content });
    invoke("hide_overlay");
    return;
  }
  if (action === "reveal") {
    invoke("reveal_in_explorer", { path: r.path });
  } else {
    invoke("open_file", { path: r.path });
  }
}

// ---- Add-project mode ----

function nameFromPath(path) {
  const cleaned = path.replace(/[\\/]+$/, "");
  return cleaned.split(/[\\/]/).pop() || "";
}

// The name field and Add button only appear once a folder is actually
// picked — showing them up front (as an always-present text box) read as
// "is this a search box?" rather than "name your new project", since it
// looked like just another input sitting next to a drop-zone.
function renderAddProjectView(status) {
  const hasPath = Boolean(pendingProjectPath);
  const form = hasPath
    ? `<div id="add-project-form">
        <input id="project-name-input" placeholder="Name this project…" autocomplete="off" spellcheck="false" value="${escapeHtml(nameFromPath(pendingProjectPath))}" />
        <button id="add-project-confirm">Add</button>
      </div>`
    : "";

  resultsEl.innerHTML = `
    <div id="add-project-view">
      <div id="drop-zone" class="${hasPath ? "has-path" : ""}">
        <div class="drop-icon">📁</div>
        <div class="drop-text">${hasPath ? "Folder selected — click to change" : "Drop a folder here or click to browse"}</div>
        ${hasPath ? `<div class="drop-path">${escapeHtml(pendingProjectPath)}</div>` : ""}
      </div>
      ${form}
      <div id="add-project-status" class="${status?.kind || ""}">${status?.text || ""}</div>
    </div>`;
}

function enterAddProjectMode() {
  mode = "add-project";
  pendingProjectPath = null;
  addProjectBtn.classList.add("active");
  renderAddProjectView();
}

// Leaves add-project mode without touching the search box's value — used
// when the user starts interacting with the search input directly, which
// should hand control back to search immediately, not lose what they typed.
function leaveAddProjectMode() {
  mode = "search";
  pendingProjectPath = null;
  addProjectBtn.classList.remove("active");
}

function exitAddProjectMode() {
  leaveAddProjectMode();
  clearSearch();
}

async function pickFolder() {
  const path = await invoke("pick_folder");
  if (path) {
    pendingProjectPath = path;
    renderAddProjectView();
    resultsEl.querySelector("#project-name-input")?.focus();
  }
}

async function confirmAddProject() {
  const nameInput = resultsEl.querySelector("#project-name-input");
  const name = nameInput?.value.trim();
  if (!pendingProjectPath || !name) return;

  const confirmBtn = resultsEl.querySelector("#add-project-confirm");
  if (confirmBtn) confirmBtn.disabled = true;
  renderAddProjectView({ kind: "", text: "Adding…" });

  try {
    await invoke("add_project", { name, path: pendingProjectPath });
    renderAddProjectView({ kind: "success", text: "✓ Added — scanning in the background" });
    setTimeout(exitAddProjectMode, 1100);
  } catch (e) {
    renderAddProjectView({ kind: "error", text: String(e) });
    resultsEl.querySelector("#project-name-input")?.focus();
  }
}

// ---- Scan progress (from add_project's background scan) ----

// A folder with a few hundred files scans in well under a second — without
// a floor here, "Scanning…" would flash and swap to "Indexed" before
// anyone could actually read it, which is exactly what looked broken.
const MIN_SCANNING_VISIBLE_MS = 700;
const DONE_VISIBLE_MS = 3500;
let scanStartedAtMs = 0;

function showScanStatus(text, done) {
  clearTimeout(scanBarHideTimer);
  scanBarTextEl.textContent = text;
  scanBarEl.classList.add("visible");
  scanBarEl.classList.toggle("done", Boolean(done));
  if (done) {
    scanBarHideTimer = setTimeout(() => scanBarEl.classList.remove("visible"), DONE_VISIBLE_MS);
  }
}

listen("scan-progress", (event) => {
  const { project, status, indexed } = event.payload;
  if (status === "started") {
    scanStartedAtMs = performance.now();
    showScanStatus(`Scanning "${project}"…`, false);
    return;
  }

  const showFinished = () => {
    const count = typeof indexed === "number" ? `${indexed} file${indexed === 1 ? "" : "s"}` : "files";
    showScanStatus(`✓ Indexed ${count} in "${project}"`, true);
  };
  const elapsed = performance.now() - scanStartedAtMs;
  const remaining = MIN_SCANNING_VISIBLE_MS - elapsed;
  if (remaining > 0) {
    setTimeout(showFinished, remaining);
  } else {
    showFinished();
  }
});

// ---- Input wiring ----

// Touching the search box — clicking into it or typing — always means "I
// want to search", so it backs out of add-project mode instead of eating
// the interaction.
queryEl.addEventListener("focus", () => {
  if (mode === "add-project") {
    leaveAddProjectMode();
    renderResults();
  }
});

queryEl.addEventListener("input", () => {
  if (mode === "add-project") {
    leaveAddProjectMode();
  }
  updateClearButton();
  clearTimeout(debounceTimer);
  const q = queryEl.value;
  debounceTimer = setTimeout(() => runSearch(q), 120);
});

clearBtn.addEventListener("click", () => clearSearch());

addProjectBtn.addEventListener("click", () => {
  if (mode === "add-project") {
    exitAddProjectMode();
  } else {
    enterAddProjectMode();
  }
});

// Attached to document, not just the input: clicking a result row shouldn't
// be able to strand keyboard navigation by moving focus off the input.
document.addEventListener("keydown", (e) => {
  if (mode === "add-project") {
    if (e.key === "Escape") {
      e.preventDefault();
      exitAddProjectMode();
    } else if (e.key === "Enter" && document.activeElement?.id === "project-name-input") {
      e.preventDefault();
      confirmAddProject();
    }
    return;
  }

  if (e.key === "ArrowDown") {
    e.preventDefault();
    if (results.length) {
      selected = (selected + 1) % results.length;
      updateSelection();
    }
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    if (results.length) {
      selected = (selected - 1 + results.length) % results.length;
      updateSelection();
    }
  } else if (e.key === "Escape") {
    e.preventDefault();
    // First Escape clears an active query (instant-clear hotkey); only a
    // second press — once there's nothing left to clear — hides Lumos.
    if (queryEl.value.length > 0) {
      clearSearch();
    } else {
      invoke("hide_overlay");
    }
  } else if (e.key === "Enter") {
    e.preventDefault();
    activate(results[selected], e.ctrlKey ? "reveal" : "open");
  }
});

resultsEl.addEventListener("mouseover", (e) => {
  if (mode !== "search") return;
  const row = e.target.closest(".result");
  if (!row) return;
  const idx = Number(row.dataset.index);
  if (idx !== selected) {
    selected = idx;
    updateSelection();
  }
});

resultsEl.addEventListener("click", (e) => {
  if (mode === "add-project") {
    if (e.target.closest("#drop-zone")) {
      pickFolder();
    } else if (e.target.closest("#add-project-confirm")) {
      confirmAddProject();
    }
    return;
  }
  const row = e.target.closest(".result");
  if (!row) return;
  const idx = Number(row.dataset.index);
  activate(results[idx], e.ctrlKey ? "reveal" : "open");
});

// ---- OS-level drag and drop (works from anywhere in the window, not just
// the drop-zone — dropping a folder auto-opens the add-project panel) ----
// Uses the documented onDragDropEvent API (payload.type: "over" | "drop" |
// anything else for cancel/leave) rather than guessing raw event-name
// strings, which aren't part of the stable public surface.

const dragDropSource =
  window.__TAURI__.webview?.getCurrentWebview?.() ??
  window.__TAURI__.webviewWindow?.getCurrentWebviewWindow?.();

if (dragDropSource?.onDragDropEvent) {
  dragDropSource.onDragDropEvent((event) => {
    const payload = event.payload;
    if (payload.type === "over") {
      dragVeilEl.classList.add("visible");
    } else if (payload.type === "drop") {
      dragVeilEl.classList.remove("visible");
      const paths = payload.paths;
      if (Array.isArray(paths) && paths.length > 0) {
        pendingProjectPath = paths[0];
        if (mode !== "add-project") {
          mode = "add-project";
          addProjectBtn.classList.add("active");
        }
        renderAddProjectView();
        resultsEl.querySelector("#project-name-input")?.focus();
      }
    } else {
      dragVeilEl.classList.remove("visible");
    }
  });
} else {
  console.warn("drag-drop API unavailable — folder picker button still works");
}

listen("overlay-shown", () => {
  // Force a reflow between remove/add so rapid re-opens restart the CSS
  // animation instead of a no-op (removing then re-adding the same class
  // in one frame wouldn't otherwise retrigger it).
  shellEl.classList.remove("entering");
  void shellEl.offsetWidth;
  shellEl.classList.add("entering");

  if (mode === "add-project") return; // preserve in-progress add flow across toggles
  queryEl.value = "";
  currentQuery = "";
  results = [];
  selected = 0;
  updateClearButton();
  renderResults();
  queryEl.focus();
});

renderResults();
