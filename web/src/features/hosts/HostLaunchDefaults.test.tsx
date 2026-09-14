import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { HostLaunchDefaults, parseLaunchArgs } from "./HostLaunchDefaults";

describe("HostLaunchDefaults", () => {
  it("splits on whitespace without shell-parsing", () => {
    expect(parseLaunchArgs("  --effort   high  ")).toEqual(["--effort", "high"]);
    expect(parseLaunchArgs("")).toEqual([]);
    // Quotes and pipes are ordinary characters in an argv array; the Node
    // execs directly, so nothing here is interpreted by a shell.
    expect(parseLaunchArgs(`--name "a b"`)).toEqual(["--name", '"a', 'b"']);
  });

  it("saves both defaults as one patch and only once they change", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();
    render(
      <HostLaunchDefaults args={["--effort", "high"]} binaryPath="/srv/claude" onSave={onSave} />,
    );
    // Nothing edited yet, so there is nothing to save.
    expect(screen.getByTestId("host-default-save")).toBeDisabled();

    await user.clear(screen.getByTestId("host-default-args"));
    await user.type(screen.getByTestId("host-default-args"), "--effort max");
    await user.click(screen.getByTestId("host-default-save"));
    expect(onSave).toHaveBeenCalledWith({
      defaultLaunchArgs: ["--effort", "max"],
      claudeBinaryPath: "/srv/claude",
    });
  });

  it("clears a default with null rather than an empty value", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();
    render(<HostLaunchDefaults args={["--ide"]} binaryPath="/srv/claude" onSave={onSave} />);
    await user.clear(screen.getByTestId("host-default-args"));
    await user.clear(screen.getByTestId("host-default-binary"));
    await user.click(screen.getByTestId("host-default-save"));
    // `null` removes the row's default. An empty string would store a
    // present-but-empty value, which the Hub treats as a different thing.
    expect(onSave).toHaveBeenCalledWith({
      defaultLaunchArgs: null,
      claudeBinaryPath: null,
    });
  });

  it("shows the probed claude path as the placeholder when no default is set", () => {
    render(
      <HostLaunchDefaults
        args={undefined}
        binaryPath={undefined}
        probedBinaryPath="/usr/local/bin/claude"
        onSave={vi.fn()}
      />,
    );
    expect(screen.getByTestId("host-default-binary")).toHaveValue("");
    expect(screen.getByTestId("host-default-binary")).toHaveAttribute(
      "placeholder",
      "/usr/local/bin/claude",
    );
  });
});


it("saves a renderer change as the per-host default", async () => {
  const onSave = vi.fn();
  const user = userEvent.setup();
  render(<HostLaunchDefaults args={undefined} binaryPath={undefined} tui="default" onSave={onSave} />);
  expect(screen.getByTestId("host-default-tui")).toHaveValue("default");
  await user.selectOptions(screen.getByTestId("host-default-tui"), "fullscreen");
  await user.click(screen.getByTestId("host-default-save"));
  expect(onSave).toHaveBeenCalledWith({ defaultLaunchArgs: null, claudeBinaryPath: null, defaultTui: "fullscreen" });
});
