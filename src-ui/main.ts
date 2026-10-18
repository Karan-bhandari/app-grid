import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface AppRef {
  bundle_id: string;
  name: string;
  path: string;
}

interface Folder {
  id: string;
  name: string;
  apps: AppRef[];
}

type LayoutItem =
  | { type: "app"; bundle_id: string; name: string; path: string }
  | { type: "folder"; id: string; name: string; apps: AppRef[] };

interface GridConfig {
  columns: number;
  rows: number;
  icon_size_px: number;
  header_offset_pt: number;
  drop_folder_delay_ms: number;
}

interface Page {
  page_index: number;
  items: LayoutItem[];
}

interface LayoutConfig {
  version: number;
  grid_config: GridConfig;
  pages: Page[];
  hidden_bundle_ids: string[];
}

interface FolderKeyEvent {
  key: string;
  key_code: number;
  is_enter: boolean;
  is_escape: boolean;
  is_backspace: boolean;
}

let currentLayout: LayoutConfig | null = null;
let currentPageIndex = 0;
let iconCacheMap = new Map<string, string>();

// DOM Elements
const pagesContainer = document.getElementById("pages-container")!;
const paginationDots = document.getElementById("pagination-dots")!;
const btnNewFolder = document.getElementById("btn-new-folder")!;
const btnToggleNative = document.getElementById("btn-toggle-native")!;
const folderPopover = document.getElementById("folder-popover")!;
const folderTitleInput = document.getElementById("folder-title-input") as HTMLInputElement;
const folderGrid = document.getElementById("folder-grid")!;
const btnCloseFolder = document.getElementById("btn-close-folder")!;

// Drag and Drop State Machine
interface DragState {
  isDragging: boolean;
  startX: number;
  startY: number;
  slotShifted: boolean;
  item: LayoutItem;
  fromPage: number;
  fromIndex: number;
  fromFolderId?: string;
  currentHoverCell: { page: number; index: number } | null;
  hoverZone: "outer" | "inner" | null;
  hoverStartTime: number;
  mergeCandidateTimeout: number | null;
  mergeLockTimeout: number | null;
  mergeLocked: boolean;
  edgeScrollTimeout: number | null;
  popoverExitTimeout: number | null;
}

let dragState: DragState | null = null;
let dragGhost: HTMLDivElement | null = null;
let activeFolderPopoverId: string | null = null;
let justFinishedDrag = false;
let cancelFolderRename = false;
let isEditingFolderTitle = false;
let selectedSlotIndex: number | null = null;
let selectedFolderAppIndex: number | null = null;

// Initialize UI
async function init() {
  await loadLayout();

  // Create drag ghost element
  dragGhost = document.createElement("div");
  dragGhost.id = "drag-ghost";
  document.body.appendChild(dragGhost);

  // Wheel swipe pagination (1 page per swipe gesture, absorbing trackpad inertia)
  let wheelAccumulator = 0;
  let wheelResetTimer: number | null = null;
  let wheelLockedDirection = 0;
  let wheelLockTime = 0;
  let lastAbsDelta = 0;

  window.addEventListener("wheel", (e) => {
    if (activeFolderPopoverId) return;

    const delta = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
    if (delta === 0) return;

    const now = performance.now();
    const dir = Math.sign(delta);
    const absDelta = Math.abs(delta);

    if (wheelResetTimer) clearTimeout(wheelResetTimer);
    wheelResetTimer = window.setTimeout(() => {
      wheelAccumulator = 0;
      wheelLockedDirection = 0;
      lastAbsDelta = 0;
    }, 140);

    if (wheelLockedDirection !== 0) {
      // Unlock if user reverses scroll direction or initiates a distinct new flick after 250ms
      const isNewFlick = now - wheelLockTime > 250 && absDelta > lastAbsDelta + 14;
      if (dir !== wheelLockedDirection || isNewFlick) {
        wheelLockedDirection = 0;
        wheelAccumulator = 0;
      } else {
        lastAbsDelta = absDelta;
        return;
      }
    }

    lastAbsDelta = absDelta;
    wheelAccumulator += delta;

    if (wheelAccumulator > 45) {
      nextPage();
      wheelAccumulator = 0;
      wheelLockedDirection = 1;
      wheelLockTime = now;
    } else if (wheelAccumulator < -45) {
      prevPage();
      wheelAccumulator = 0;
      wheelLockedDirection = -1;
      wheelLockTime = now;
    }
  });

  // Global mouse move & up for drag state machine
  window.addEventListener("mousemove", handlePointerMove);
  window.addEventListener("mouseup", handlePointerUp);

  // Button clicks
  btnNewFolder.addEventListener("click", handleCreateNewFolderPrompt);
  btnToggleNative.addEventListener("click", () => {
    invoke("hide_overlay");
  });

  btnCloseFolder.addEventListener("click", closeFolderPopover);
  folderPopover.querySelector(".popover-backdrop")?.addEventListener("click", closeFolderPopover);

  // Folder rename title focus handling
  folderTitleInput.addEventListener("focus", () => {
    cancelFolderRename = false;
    isEditingFolderTitle = true;
    invoke("set_editing_folder", { editing: true });
  });

  folderTitleInput.addEventListener("blur", () => {
    isEditingFolderTitle = false;
    if (!cancelFolderRename) {
      saveFolderRename();
    }
    cancelFolderRename = false;
    invoke("set_editing_folder", { editing: false });
  });

  folderTitleInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      folderTitleInput.blur();
    } else if (e.key === "Escape") {
      cancelFolderRename = true;
      if (activeFolderPopoverId && currentLayout) {
        const folder = findFolder(activeFolderPopoverId);
        if (folder) folderTitleInput.value = folder.name;
      }
      folderTitleInput.blur();
    }
  });

  // Listen for backend live updates
  listen<LayoutConfig>("layout_updated", (event) => {
    currentLayout = event.payload;
    renderGrid();
  });

  listen("overlay_hidden", () => {
    selectedSlotIndex = null;
    selectedFolderAppIndex = null;
    updateKeyboardSelectionUI();
    closeFolderPopover();
  });

  listen<FolderKeyEvent>("folder_key_event", (event) => {
    const data = event.payload;

    if (activeFolderPopoverId && isEditingFolderTitle) {
      if (data.is_enter) {
        folderTitleInput.blur();
      } else if (data.is_escape) {
        cancelFolderRename = true;
        if (currentLayout) {
          const folder = findFolder(activeFolderPopoverId);
          if (folder) folderTitleInput.value = folder.name;
        }
        folderTitleInput.blur();
      } else if (data.is_backspace) {
        folderTitleInput.value = folderTitleInput.value.slice(0, -1);
      } else if (data.key && data.key.length === 1) {
        folderTitleInput.value += data.key;
      }
      return;
    }

    handleGridKeyboardEvent(data);
  });
}

function handleGridKeyboardEvent(data: FolderKeyEvent) {
  if (!currentLayout) return;

  if (activeFolderPopoverId) {
    const folder = findFolder(activeFolderPopoverId);
    if (!folder || folder.apps.length === 0) return;
    const len = folder.apps.length;
    const cols = 4;

    if (data.is_enter) {
      const idx = selectedFolderAppIndex ?? 0;
      const app = folder.apps[idx];
      if (app) {
        const appEls = folderGrid.querySelectorAll(".grid-item");
        const el = (appEls[idx] as HTMLElement) || folderGrid;
        closeFolderPopover();
        handleLaunchApp(app.path, el);
      }
      return;
    }

    if (selectedFolderAppIndex === null) {
      selectedFolderAppIndex = 0;
    } else if (data.key === "ArrowRight") {
      selectedFolderAppIndex = Math.min(len - 1, selectedFolderAppIndex + 1);
    } else if (data.key === "ArrowLeft") {
      selectedFolderAppIndex = Math.max(0, selectedFolderAppIndex - 1);
    } else if (data.key === "ArrowDown") {
      selectedFolderAppIndex = Math.min(len - 1, selectedFolderAppIndex + cols);
    } else if (data.key === "ArrowUp") {
      selectedFolderAppIndex = Math.max(0, selectedFolderAppIndex - cols);
    }
    updateKeyboardSelectionUI();
    return;
  }

  const page = currentLayout.pages[currentPageIndex];
  if (!page || page.items.length === 0) return;
  const len = page.items.length;
  const cols = currentLayout.grid_config.columns || 7;

  if (data.is_enter) {
    if (selectedSlotIndex === null) return;
    const item = page.items[selectedSlotIndex];
    if (!item) return;
    const cellEl = document.querySelector(
      `.page[data-page="${currentPageIndex}"] .grid-item[data-index="${selectedSlotIndex}"]`
    ) as HTMLElement | null;
    if (item.type === "app") {
      handleLaunchApp(item.path, cellEl || pagesContainer);
    } else {
      selectedFolderAppIndex = 0;
      openFolderPopover(item);
      updateKeyboardSelectionUI();
    }
    return;
  }

  if (selectedSlotIndex === null) {
    selectedSlotIndex = 0;
  } else if (data.key === "ArrowRight") {
    const atRowEnd = (selectedSlotIndex + 1) % cols === 0 || selectedSlotIndex === len - 1;
    if (atRowEnd && currentPageIndex < currentLayout.pages.length - 1) {
      const row = Math.floor(selectedSlotIndex / cols);
      currentPageIndex++;
      updatePageTransform();
      const nextLen = currentLayout.pages[currentPageIndex].items.length;
      selectedSlotIndex = Math.min(nextLen - 1, row * cols);
    } else {
      selectedSlotIndex = Math.min(len - 1, selectedSlotIndex + 1);
    }
  } else if (data.key === "ArrowLeft") {
    const atRowStart = selectedSlotIndex % cols === 0;
    if (atRowStart && currentPageIndex > 0) {
      const row = Math.floor(selectedSlotIndex / cols);
      currentPageIndex--;
      updatePageTransform();
      const prevLen = currentLayout.pages[currentPageIndex].items.length;
      selectedSlotIndex = Math.min(prevLen - 1, row * cols + (cols - 1));
    } else {
      selectedSlotIndex = Math.max(0, selectedSlotIndex - 1);
    }
  } else if (data.key === "ArrowDown") {
    selectedSlotIndex = Math.min(len - 1, selectedSlotIndex + cols);
  } else if (data.key === "ArrowUp") {
    selectedSlotIndex = Math.max(0, selectedSlotIndex - cols);
  }

  updateKeyboardSelectionUI();
}

function updateKeyboardSelectionUI() {
  document.querySelectorAll(".grid-item.selected").forEach((el) => {
    el.classList.remove("selected");
  });

  if (activeFolderPopoverId) {
    if (selectedFolderAppIndex !== null) {
      const appEls = folderGrid.querySelectorAll(".grid-item");
      const targetEl = appEls[selectedFolderAppIndex] as HTMLElement | undefined;
      if (targetEl) {
        targetEl.classList.add("selected");
        targetEl.scrollIntoView({ block: "nearest", behavior: "smooth" });
      }
    }
  } else if (selectedSlotIndex !== null) {
    const cellEl = document.querySelector(
      `.page[data-page="${currentPageIndex}"] .grid-item[data-index="${selectedSlotIndex}"]`
    );
    cellEl?.classList.add("selected");
  }
}

async function loadLayout() {
  try {
    currentLayout = await invoke<LayoutConfig>("get_layout");
    renderGrid();
  } catch (err) {
    console.error("Failed to load layout:", err);
  }
}

function assignIconSrc(img: HTMLImageElement, bundleId: string, path: string) {
  img.decoding = "async";
  const key = `${bundleId}:${path}`;
  const cached = iconCacheMap.get(key);
  if (cached) {
    img.src = cached;
    return;
  }

  invoke<string>("get_icon_path", { bundleId, path })
    .then((rawPath) => {
      const url = convertFileSrc(rawPath);
      iconCacheMap.set(key, url);
      img.src = url;
    })
    .catch(() => {});
}

function renderGrid() {
  if (!currentLayout) return;

  pagesContainer.innerHTML = "";
  paginationDots.innerHTML = "";

  currentLayout.pages.forEach((page, pageIdx) => {
    // Render Page Dot
    const dot = document.createElement("div");
    dot.className = `dot ${pageIdx === currentPageIndex ? "active" : ""}`;
    dot.addEventListener("click", () => goToPage(pageIdx));
    paginationDots.appendChild(dot);

    // Render Page Grid
    const pageEl = document.createElement("div");
    pageEl.className = "page";
    pageEl.dataset.page = pageIdx.toString();

    page.items.forEach((item, itemIdx) => {
      const cellEl = createGridItemElement(item, pageIdx, itemIdx);
      pageEl.appendChild(cellEl);
    });

    pagesContainer.appendChild(pageEl);
  });

  updatePageTransform();
}

function createGridItemElement(item: LayoutItem, pageIdx: number, itemIdx: number): HTMLElement {
  const itemEl = document.createElement("div");
  itemEl.className = "grid-item";
  if (
    dragState?.isDragging &&
    !dragState.fromFolderId &&
    dragState.fromPage === pageIdx &&
    dragState.fromIndex === itemIdx
  ) {
    itemEl.classList.add("dragging");
  }
  itemEl.dataset.page = pageIdx.toString();
  itemEl.dataset.index = itemIdx.toString();

  const iconWrapper = document.createElement("div");
  iconWrapper.className = "icon-wrapper";

  const titleEl = document.createElement("div");
  titleEl.className = "item-title";

  if (item.type === "app") {
    titleEl.textContent = item.name;
    const img = document.createElement("img");
    img.className = "app-icon";
    img.draggable = false;
    img.alt = item.name;
    assignIconSrc(img, item.bundle_id, item.path);
    iconWrapper.appendChild(img);

    // Launch app on click
    itemEl.addEventListener("click", () => {
      if (dragState?.isDragging || justFinishedDrag) return;
      handleLaunchApp(item.path, itemEl);
    });
  } else {
    // Folder
    titleEl.textContent = item.name;
    const thumbnail = document.createElement("div");
    thumbnail.className = "folder-thumbnail";

    // Preview up to 9 apps
    const previewApps = item.apps.slice(0, 9);
    previewApps.forEach((app) => {
      const miniImg = document.createElement("img");
      miniImg.className = "folder-mini-icon";
      miniImg.draggable = false;
      assignIconSrc(miniImg, app.bundle_id, app.path);
      thumbnail.appendChild(miniImg);
    });
    iconWrapper.appendChild(thumbnail);

    // Open folder popover on click
    itemEl.addEventListener("click", () => {
      if (dragState?.isDragging || justFinishedDrag) return;
      openFolderPopover(item);
    });
  }

  itemEl.appendChild(iconWrapper);
  itemEl.appendChild(titleEl);

  // Drag start handler (mousedown)
  itemEl.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return; // Only left click
    e.preventDefault();
    startDragging(item, pageIdx, itemIdx, e);
  });

  return itemEl;
}

// Page Navigation
function updatePageTransform() {
  pagesContainer.style.transform = `translate3d(-${currentPageIndex * 100}%, 0, 0)`;

  // Update dots
  const dots = paginationDots.querySelectorAll(".dot");
  dots.forEach((dot, idx) => {
    if (idx === currentPageIndex) {
      dot.classList.add("active");
    } else {
      dot.classList.remove("active");
    }
  });
}

function goToPage(index: number) {
  if (!currentLayout) return;
  if (index >= 0 && index < currentLayout.pages.length) {
    currentPageIndex = index;
    updatePageTransform();
  }
}

function nextPage() {
  if (!currentLayout) return;
  if (currentPageIndex < currentLayout.pages.length - 1) {
    currentPageIndex++;
    updatePageTransform();
  }
}

function prevPage() {
  if (currentPageIndex > 0) {
    currentPageIndex--;
    updatePageTransform();
  }
}

// App Launching
async function handleLaunchApp(path: string, element: HTMLElement) {
  element.style.transform = "scale(0.88)";
  setTimeout(async () => {
    try {
      await invoke("launch_app", { path });
    } catch (err) {
      console.error("Failed to launch app:", err);
    }
  }, 90);
}

// Drag & Drop State Machine
function startDragging(
  item: LayoutItem,
  pageIdx: number,
  itemIdx: number,
  e: MouseEvent,
  fromFolderId?: string
) {
  dragState = {
    isDragging: false,
    startX: e.clientX,
    startY: e.clientY,
    slotShifted: false,
    item,
    fromPage: pageIdx,
    fromIndex: itemIdx,
    fromFolderId,
    currentHoverCell: null,
    hoverZone: null,
    hoverStartTime: 0,
    mergeCandidateTimeout: null,
    mergeLockTimeout: null,
    mergeLocked: false,
    edgeScrollTimeout: null,
    popoverExitTimeout: null,
  };
}

function handlePointerMove(e: MouseEvent) {
  if (!dragState) return;

  if (!dragState.isDragging) {
    const dist = Math.hypot(e.clientX - dragState.startX, e.clientY - dragState.startY);
    if (dist <= 5) return;

    dragState.isDragging = true;
    const { item, fromPage, fromIndex, fromFolderId } = dragState;

    if (dragGhost) {
      dragGhost.innerHTML = "";
      const ghostImg = document.createElement("img");
      if (item.type === "app") {
        assignIconSrc(ghostImg, item.bundle_id, item.path);
      }
      dragGhost.appendChild(ghostImg);
      dragGhost.classList.add("visible");
      updateGhostPosition(e.clientX, e.clientY);
    }

    if (!fromFolderId) {
      const draggedEl = document.querySelector(
        `[data-page="${fromPage}"][data-index="${fromIndex}"]`
      );
      draggedEl?.classList.add("dragging");
    }
  }

  updateGhostPosition(e.clientX, e.clientY);

  // If dragging out of an open folder popover, close popover after 200ms outside .popover-content
  if (dragState.fromFolderId && activeFolderPopoverId) {
    const popoverContent = folderPopover.querySelector(".popover-content");
    if (popoverContent) {
      const pRect = popoverContent.getBoundingClientRect();
      const isOutside =
        e.clientX < pRect.left ||
        e.clientX > pRect.right ||
        e.clientY < pRect.top ||
        e.clientY > pRect.bottom;
      if (isOutside) {
        if (!dragState.popoverExitTimeout) {
          dragState.popoverExitTimeout = window.setTimeout(() => {
            closeFolderPopover();
            if (dragState) dragState.popoverExitTimeout = null;
          }, 200);
        }
      } else if (dragState.popoverExitTimeout) {
        clearTimeout(dragState.popoverExitTimeout);
        dragState.popoverExitTimeout = null;
      }
    }
    return;
  }

  // Check 15px edge gutter for cross-page navigation (UI-05)
  const windowWidth = window.innerWidth;
  if (e.clientX > windowWidth - 25) {
    startEdgeScroll(1);
  } else if (e.clientX < 25) {
    startEdgeScroll(-1);
  } else {
    cancelEdgeScroll();
  }

  // Hit-test cells under cursor
  const cellUnder = document
    .elementFromPoint(e.clientX, e.clientY)
    ?.closest(".grid-item") as HTMLElement | null;

  if (!cellUnder || cellUnder.dataset.page === undefined) {
    clearMergeHover();
    return;
  }

  const targetPage = parseInt(cellUnder.dataset.page || "0");
  const targetIndex = parseInt(cellUnder.dataset.index || "0");

  // Avoid hovering self
  if (
    !dragState.fromFolderId &&
    targetPage === dragState.fromPage &&
    targetIndex === dragState.fromIndex
  ) {
    clearMergeHover();
    return;
  }

  // Calculate Dual-Zone geometry (Section 6.2)
  const rect = cellUnder.getBoundingClientRect();
  const relX = (e.clientX - rect.left) / rect.width;
  const relY = (e.clientY - rect.top) / rect.height;

  // Inner Core is 40% x 40% center box (between 0.30 and 0.70)
  const inInnerCore = relX >= 0.3 && relX <= 0.7 && relY >= 0.3 && relY <= 0.7;

  const shiftActive = e.shiftKey;
  const optionActive = e.altKey;

  // Shift modifier forces Reorder Only (UI-04)
  if (shiftActive) {
    clearMergeHover();
    performSlotShift(targetPage, targetIndex);
    return;
  }

  // Option modifier forces Folder Merge Only immediately (UI-03)
  if (optionActive && inInnerCore) {
    clearMergeHover();
    dragState.currentHoverCell = { page: targetPage, index: targetIndex };
    dragState.hoverZone = "inner";
    dragState.mergeLocked = true;
    cellUnder.classList.add("merge-locked");
    return;
  }

  if (inInnerCore) {
    // Entered Inner Core
    if (
      !dragState.currentHoverCell ||
      dragState.currentHoverCell.page !== targetPage ||
      dragState.currentHoverCell.index !== targetIndex ||
      dragState.hoverZone !== "inner"
    ) {
      clearMergeHover();
      dragState.currentHoverCell = { page: targetPage, index: targetIndex };
      dragState.hoverZone = "inner";
      dragState.hoverStartTime = Date.now();

      // At 80ms: scale 1.08x + preview border
      dragState.mergeCandidateTimeout = window.setTimeout(() => {
        cellUnder.classList.add("merge-candidate");
      }, 80);

      // At 250ms: merge state locks in (visual pulse)
      dragState.mergeLockTimeout = window.setTimeout(() => {
        dragState!.mergeLocked = true;
        cellUnder.classList.remove("merge-candidate");
        cellUnder.classList.add("merge-locked");
      }, 250);
    }
  } else {
    // In Outer Border Ring: after 80ms hover, trigger Grid Slot Swap / Shift (UI-01)
    if (
      !dragState.currentHoverCell ||
      dragState.currentHoverCell.page !== targetPage ||
      dragState.currentHoverCell.index !== targetIndex ||
      dragState.hoverZone !== "outer"
    ) {
      clearMergeHover();
      dragState.currentHoverCell = { page: targetPage, index: targetIndex };
      dragState.hoverZone = "outer";
      dragState.hoverStartTime = Date.now();

      dragState.mergeCandidateTimeout = window.setTimeout(() => {
        performSlotShift(targetPage, targetIndex);
      }, 80);
    }
  }
}

function clearMergeHover() {
  if (dragState) {
    if (dragState.mergeCandidateTimeout) {
      clearTimeout(dragState.mergeCandidateTimeout);
      dragState.mergeCandidateTimeout = null;
    }
    if (dragState.mergeLockTimeout) {
      clearTimeout(dragState.mergeLockTimeout);
      dragState.mergeLockTimeout = null;
    }
    dragState.mergeLocked = false;
    dragState.hoverZone = null;
    dragState.currentHoverCell = null;
  }

  document.querySelectorAll(".merge-candidate, .merge-locked").forEach((el) => {
    el.classList.remove("merge-candidate", "merge-locked");
  });
}

function updateGhostPosition(x: number, y: number) {
  if (dragGhost) {
    dragGhost.style.left = `${x}px`;
    dragGhost.style.top = `${y}px`;
  }
}

function startEdgeScroll(dir: number) {
  if (!dragState || dragState.edgeScrollTimeout) return;
  dragState.edgeScrollTimeout = window.setTimeout(() => {
    if (dir > 0) nextPage();
    else prevPage();
    dragState!.edgeScrollTimeout = null;
  }, 350);
}

function cancelEdgeScroll() {
  if (dragState?.edgeScrollTimeout) {
    clearTimeout(dragState.edgeScrollTimeout);
    dragState.edgeScrollTimeout = null;
  }
}

async function handlePointerUp() {
  if (!dragState || !dragState.isDragging) {
    dragState = null;
    return;
  }

  justFinishedDrag = true;
  window.setTimeout(() => {
    justFinishedDrag = false;
  }, 60);

  if (dragGhost) {
    dragGhost.classList.remove("visible");
  }

  if (dragState.popoverExitTimeout) {
    clearTimeout(dragState.popoverExitTimeout);
    dragState.popoverExitTimeout = null;
  }

  const { item, fromPage, fromIndex, fromFolderId, currentHoverCell, mergeLocked, slotShifted } =
    dragState;

  // Cleanup visual feedback
  clearMergeHover();
  cancelEdgeScroll();
  document.querySelectorAll(".dragging").forEach((el) => el.classList.remove("dragging"));

  // Check if dropped onto a merge target (UI-02, UI-03)
  if (mergeLocked && currentHoverCell && currentLayout) {
    const targetItem = currentLayout.pages[currentHoverCell.page]?.items[currentHoverCell.index];
    if (targetItem) {
      if (targetItem.type === "folder") {
        // Add dragged item to existing folder
        if (item.type === "app") {
          targetItem.apps.push({
            bundle_id: item.bundle_id,
            name: item.name,
            path: item.path,
          });
          // Remove from origin
          removeItemFromLayout(fromPage, fromIndex, fromFolderId);
          await saveCurrentLayout();
        }
      } else if (targetItem.type === "app" && item.type === "app") {
        // Create new folder containing [targetItem, item]
        const appIds = [targetItem.bundle_id, item.bundle_id];
        try {
          currentLayout = await invoke<LayoutConfig>("create_folder", {
            name: "Untitled Folder",
            appIds,
          });
          renderGrid();
        } catch (err) {
          console.error("Failed to create folder:", err);
        }
      }
    }
  } else if ((currentHoverCell || slotShifted || fromFolderId) && currentLayout) {
    // Normal reorder drop or drag-out from folder
    const destPage = currentHoverCell?.page ?? fromPage;
    const destIndex = currentHoverCell?.index ?? fromIndex;
    const itemId = item.type === "app" ? item.bundle_id : item.id;
    try {
      currentLayout = await invoke<LayoutConfig>("move_item", {
        itemId,
        targetPage: destPage,
        targetIndex: destIndex,
      });
      renderGrid();
    } catch (err) {
      console.error("Failed to move item:", err);
    }
  }

  dragState = null;
}

function performSlotShift(targetPage: number, targetIndex: number) {
  if (!currentLayout || !dragState) return;

  if (dragState.fromFolderId) {
    dragState.currentHoverCell = { page: targetPage, index: targetIndex };
    dragState.slotShifted = true;
    return;
  }

  const page = currentLayout.pages[dragState.fromPage];
  if (!page) return;

  const [movedItem] = page.items.splice(dragState.fromIndex, 1);
  const targetPageObj = currentLayout.pages[targetPage];
  if (!targetPageObj) return;

  targetPageObj.items.splice(targetIndex, 0, movedItem);

  dragState.fromPage = targetPage;
  dragState.fromIndex = targetIndex;
  dragState.slotShifted = true;

  renderGrid();
}

function removeItemFromLayout(pageIdx: number, itemIdx: number, folderId?: string) {
  if (!currentLayout) return;
  if (folderId) {
    const folder = findFolder(folderId);
    if (folder) {
      folder.apps.splice(itemIdx, 1);
      // Auto-dissolve rule if folder has <= 1 app
      if (folder.apps.length <= 1) {
        for (const page of currentLayout.pages) {
          const fIdx = page.items.findIndex((it) => it.type === "folder" && it.id === folderId);
          if (fIdx !== -1) {
            const remaining = folder.apps.map((a) => ({
              type: "app" as const,
              ...a,
            }));
            page.items.splice(fIdx, 1, ...remaining);
            break;
          }
        }
      }
    }
  } else {
    currentLayout.pages[pageIdx]?.items.splice(itemIdx, 1);
  }
}

async function saveCurrentLayout() {
  if (!currentLayout) return;
  try {
    currentLayout = await invoke<LayoutConfig>("save_layout", {
      layout: currentLayout,
    });
    renderGrid();
  } catch (err) {
    console.error("Error saving layout:", err);
  }
}

// Folder Popover Behavior (Section 6.3)
function openFolderPopover(folder: Folder) {
  activeFolderPopoverId = folder.id;
  folderTitleInput.value = folder.name;
  folderGrid.innerHTML = "";

  folder.apps.forEach((app, appIdx) => {
    const appEl = document.createElement("div");
    appEl.className = "grid-item";

    const iconWrapper = document.createElement("div");
    iconWrapper.className = "icon-wrapper";
    const img = document.createElement("img");
    img.className = "app-icon";
    img.draggable = false;
    img.alt = app.name;
    assignIconSrc(img, app.bundle_id, app.path);
    iconWrapper.appendChild(img);

    const titleEl = document.createElement("div");
    titleEl.className = "item-title";
    titleEl.textContent = app.name;

    appEl.appendChild(iconWrapper);
    appEl.appendChild(titleEl);

    // Launch app on click
    appEl.addEventListener("click", () => {
      if (dragState?.isDragging || justFinishedDrag) return;
      closeFolderPopover();
      handleLaunchApp(app.path, appEl);
    });

    // Allow dragging app out of folder onto main grid
    appEl.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      const appItem: LayoutItem = {
        type: "app",
        bundle_id: app.bundle_id,
        name: app.name,
        path: app.path,
      };
      startDragging(appItem, currentPageIndex, appIdx, e, folder.id);
    });

    folderGrid.appendChild(appEl);
  });

  folderPopover.classList.remove("hidden");
}

function closeFolderPopover() {
  if (!activeFolderPopoverId) return;
  folderPopover.classList.add("hidden");
  activeFolderPopoverId = null;
  invoke("set_editing_folder", { editing: false });
}

async function saveFolderRename() {
  if (!activeFolderPopoverId) return;
  const newName = folderTitleInput.value.trim();
  if (newName.length === 0) return;

  try {
    currentLayout = await invoke<LayoutConfig>("rename_folder", {
      folderId: activeFolderPopoverId,
      newName,
    });
    renderGrid();
  } catch (err) {
    console.error("Failed to rename folder:", err);
  }
}

function findFolder(folderId: string): Folder | null {
  if (!currentLayout) return null;
  for (const page of currentLayout.pages) {
    for (const item of page.items) {
      if (item.type === "folder" && item.id === folderId) {
        return item;
      }
    }
  }
  return null;
}

async function handleCreateNewFolderPrompt() {
  const page = currentLayout?.pages[currentPageIndex];
  if (!page) return;

  const standaloneApps = page.items.filter(
    (it): it is Extract<LayoutItem, { type: "app" }> => it.type === "app"
  );
  if (standaloneApps.length < 2) return;

  const [app1, app2] = standaloneApps;
  try {
    currentLayout = await invoke<LayoutConfig>("create_folder", {
      name: "New Folder",
      appIds: [app1.bundle_id, app2.bundle_id],
    });
    renderGrid();

    // Open newly created folder popover and focus title input for inline rename
    const updatedPage = currentLayout.pages[currentPageIndex];
    const createdFolder = updatedPage?.items.find(
      (it): it is Extract<LayoutItem, { type: "folder" }> =>
        it.type === "folder" && it.apps.some((a) => a.bundle_id === app1.bundle_id)
    );
    if (createdFolder) {
      openFolderPopover(createdFolder);
      folderTitleInput.focus();
      folderTitleInput.select();
    }
  } catch (err) {
    console.error("Failed to create folder:", err);
  }
}

// Start
init();
