import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { WorkspaceRegistration } from "../features/workspaces/WorkspaceRegistration";
import { api } from "../lib/api";
import * as store from "../lib/store";
import { Sheet } from "./Sheet";
import { focusableIn } from "./useFocusTrap";

/** A trigger plus the overlay it opens — the shape every caller of Sheet has. */
function Harness({ variant = "popover", withTerminal = false }: { variant?: "popover" | "sheet"; withTerminal?: boolean }) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement | null>(null);
  return (
    <div>
      <button type="button" ref={trigger} data-testid="trigger" aria-expanded={open} onClick={() => setOpen(true)}>
        筛选
      </button>
      <button type="button" data-testid="outside">
        页面上的其他按钮
      </button>
      <Sheet open={open} onClose={() => setOpen(false)} variant={variant} labelledBy="overlay-title" returnFocusRef={trigger} testId="overlay">
        <h2 id="overlay-title">筛选会话</h2>
        <button type="button" data-testid="first">
          第一项
        </button>
        {withTerminal ? (
          <div className="xterm" data-testid="terminal">
            <textarea data-testid="terminal-input" aria-label="Terminal input" />
          </div>
        ) : null}
        <button type="button" data-testid="last">
          最后一项
        </button>
      </Sheet>
    </div>
  );
}

describe("useFocusTrap via Sheet", () => {
  it("names itself and reports modal semantics when open", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    expect(screen.queryByTestId("overlay")).toBeNull();
    await user.click(screen.getByTestId("trigger"));
    const overlay = screen.getByTestId("overlay");
    expect(overlay).toHaveAttribute("role", "dialog");
    expect(overlay).toHaveAttribute("aria-modal", "true");
    expect(overlay).toHaveAccessibleName("筛选会话");
  });

  it("moves focus into the overlay on open", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByTestId("trigger"));
    expect(screen.getByTestId("first")).toHaveFocus();
  });

  it("honours an explicit initial focus target", async () => {
    function WithInitial() {
      const [open, setOpen] = useState(false);
      const initial = useRef<HTMLButtonElement | null>(null);
      return (
        <>
          <button type="button" data-testid="trigger" onClick={() => setOpen(true)}>
            打开
          </button>
          <Sheet open={open} onClose={() => setOpen(false)} initialFocusRef={initial} testId="overlay">
            <button type="button" data-testid="first">
              第一项
            </button>
            <button type="button" ref={initial} data-testid="wanted">
              想要的焦点
            </button>
          </Sheet>
        </>
      );
    }
    const user = userEvent.setup();
    render(<WithInitial />);
    await user.click(screen.getByTestId("trigger"));
    expect(screen.getByTestId("wanted")).toHaveFocus();
  });

  it("cycles Tab from the last item back to the first", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByTestId("trigger"));
    screen.getByTestId("last").focus();
    await user.tab();
    expect(screen.getByTestId("first")).toHaveFocus();
  });

  it("cycles Shift+Tab from the first item to the last, never onto the page behind", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByTestId("trigger"));
    expect(screen.getByTestId("first")).toHaveFocus();
    await user.tab({ shift: true });
    expect(screen.getByTestId("last")).toHaveFocus();
    expect(screen.getByTestId("outside")).not.toHaveFocus();
  });

  it("closes on Escape", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByTestId("trigger"));
    await user.keyboard("{Escape}");
    expect(screen.queryByTestId("overlay")).toBeNull();
  });

  it("returns focus to the trigger on close", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const trigger = screen.getByTestId("trigger");
    await user.click(trigger);
    expect(trigger).not.toHaveFocus();
    await user.keyboard("{Escape}");
    expect(trigger).toHaveFocus();
  });

  it("reports open state on the trigger for assistive tech", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const trigger = screen.getByTestId("trigger");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    await user.keyboard("{Escape}");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
  });

  it("renders both variants through the same contract", async () => {
    const user = userEvent.setup();
    render(<Harness variant="sheet" />);
    await user.click(screen.getByTestId("trigger"));
    const overlay = screen.getByTestId("overlay");
    expect(overlay).toHaveAttribute("data-variant", "sheet");
    expect(overlay).toHaveAttribute("aria-modal", "true");
    expect(screen.getByTestId("first")).toHaveFocus();
  });

  // UX plan §4 risk 1: an attached terminal owns Escape and Tab as bytes for
  // the native process. The overlay must not act on keys that originate there.
  describe("attached terminal passthrough", () => {
    it("does not close when Escape comes from inside .xterm", async () => {
      const user = userEvent.setup();
      render(<Harness withTerminal />);
      await user.click(screen.getByTestId("trigger"));
      screen.getByTestId("terminal-input").focus();
      await user.keyboard("{Escape}");
      expect(screen.getByTestId("overlay")).toBeVisible();
    });

    it("does not re-aim Tab pressed inside .xterm", async () => {
      const user = userEvent.setup();
      render(<Harness withTerminal />);
      await user.click(screen.getByTestId("trigger"));
      const terminal = screen.getByTestId("terminal-input");
      terminal.focus();
      await user.keyboard("{Tab}");
      // The trap did not force focus to the first item; the terminal kept the key.
      expect(screen.getByTestId("first")).not.toHaveFocus();
      expect(screen.getByTestId("overlay")).toBeVisible();
    });

    it("still traps keys that come from an ordinary input in the same overlay", async () => {
      const user = userEvent.setup();
      render(<Harness withTerminal />);
      await user.click(screen.getByTestId("trigger"));
      screen.getByTestId("last").focus();
      await user.keyboard("{Escape}");
      expect(screen.queryByTestId("overlay")).toBeNull();
    });
  });

  it("attaches no window-level key listener", async () => {
    // A global listener is what would swallow Escape for every terminal on the
    // page, so assert the hook never registers one.
    const add = vi.spyOn(window, "addEventListener");
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByTestId("trigger"));
    expect(add.mock.calls.filter(([type]) => type === "keydown")).toHaveLength(0);
    add.mockRestore();
  });

  it("closes when the scrim behind the panel is pressed, but not the panel itself", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByTestId("trigger"));
    await user.click(screen.getByTestId("first"));
    expect(screen.getByTestId("overlay")).toBeVisible();
    const scrim = screen.getByTestId("overlay").parentElement!;
    await user.click(scrim);
    expect(screen.queryByTestId("overlay")).toBeNull();
  });
});

describe("portalled DirBrowser inside the New Session sheet form", () => {
  // Round 6 item 2: React synthetic events bubble THROUGH portals to React
  // ancestors, so a Tab in the portalled DirBrowser modal reached the
  // Sheet's trap and refocused the sheet. The innermost trap now owns Tab and
  // the outer trap ignores events whose DOM target is not in its container.
  const listing = {
    path: "/srv/proj",
    parent: null,
    home: "/srv/proj",
    roots: ["/srv/proj"],
    workspaces: [],
    dirs: [{ name: "alpha" }],
    truncated: false,
  };

  function SessionHarness() {
    return (
      <Sheet open onClose={() => undefined} testId="new-session-sheet" labelledBy="new-session-title">
        <h2 id="new-session-title">新建会话</h2>
        {/* A VALID session form: Enter in a real field submits and creates. */}
        <form
          data-testid="session-form"
          onSubmit={(event) => {
            event.preventDefault();
            void store.hubStore.create({ hostId: "hst_x" } as never);
          }}
        >
          <input data-testid="new-session-cwd" defaultValue="" />
          <WorkspaceRegistration hostId="hst_x" />
          <button type="submit" data-testid="new-session-start">
            启动
          </button>
        </form>
      </Sheet>
    );
  }

  async function openBrowser() {
    vi.spyOn(api, "hostDirsList").mockResolvedValue(listing);
    render(<SessionHarness />);
    const create = vi.spyOn(store.hubStore, "create").mockResolvedValue({} as never);
    await userEvent.click(screen.getByTestId("workspace-add"));
    screen.getByTestId("dir-browser");
    return { create };
  }

  it("keeps Tab and Shift+Tab in the portalled modal at both boundaries", async () => {
    const user = userEvent.setup();
    await openBrowser();
    // The modal panel is the active dialog; it lives in a portal, outside the
    // sheet panel's DOM subtree.
    const dialog = screen.getByTestId("dir-browser");
    const inModal = () => {
      const active = document.activeElement;
      return active instanceof Node && dialog.contains(active);
    };
    const visible = focusableIn(dialog as HTMLElement);
    const first = visible[0] as HTMLElement;
    const last = visible[visible.length - 1] as HTMLElement;
    first.focus();
    expect(inModal(), `first is ${document.activeElement?.getAttribute("data-testid")}`).toBe(true);

    // Forward boundary: Tab on the last control wraps to the first INSIDE
    // the modal, never landing back on a sheet/form control.
    last.focus();
    await user.tab();
    expect(
      inModal(),
      `after Tab wrap active=${document.activeElement?.getAttribute("data-testid")}`,
    ).toBe(true);
    expect(document.activeElement).not.toBe(screen.getByTestId("new-session-cwd"));
    expect(document.activeElement).not.toBe(screen.getByTestId("workspace-add"));
    expect(document.activeElement).not.toBe(screen.getByTestId("new-session-start"));

    // Backward boundary: Shift+Tab on the first wraps to the last, still in
    // the modal.
    first.focus();
    await user.tab({ shift: true });
    expect(
      inModal(),
      `after Shift+Tab wrap active=${document.activeElement?.getAttribute("data-testid")}`,
    ).toBe(true);
    expect(document.activeElement).not.toBe(screen.getByTestId("new-session-cwd"));
  });

  it("never submits the session form (hubStore.create) for Enter in the modal", async () => {
    const user = userEvent.setup();
    const { create } = await openBrowser();
    // Focus moves into the modal on open; Enter there must not create.
    await user.keyboard("{Enter}");
    screen.getByTestId("dir-browser-filter").focus();
    await user.keyboard("{Enter}");
    expect(create).not.toHaveBeenCalled();
    // The modal is still open (it did not close or hand the key outward).
    expect(screen.getByRole("dialog")).toBeTruthy();
  });
});

describe("focusableIn", () => {
  it("lists the tab order and skips what cannot take focus", () => {
    const host = document.createElement("div");
    host.innerHTML = `
      <a href="#a">link</a>
      <button>ok</button>
      <button disabled>no</button>
      <input type="hidden" />
      <input type="text" />
      <textarea></textarea>
      <div tabindex="-1">skip</div>
      <div tabindex="0">yes</div>
      <button hidden>hidden</button>
      <button style="display:none">none</button>
      <button style="visibility:hidden">invisible</button>
      <div inert><button>inside inert</button></div>`;
    document.body.append(host);
    const tags = focusableIn(host).map((element) => element.tagName.toLowerCase());
    expect(tags).toEqual(["a", "button", "input", "textarea", "div"]);
    host.remove();
  });
});
