# Tutorial: inspect an application

In this tutorial, you will start a Wayland application, capture its window, inspect its
widgets, and stop the session.

## Before you start

Install waypuppet as described in the [README](../README.md). You also need a Wayland
application. The examples use `gtk3-widget-factory`, a GTK 3 demo. Install it with your
Linux distribution's package manager, or replace it with the path to your application.
Your application must be available in the runtime that waypuppet selects.

Check the runtime:

```sh
waypuppet doctor
```

If the command reports that a runtime is missing, run `waypuppet setup`, then run
`waypuppet doctor` again.

## Start the application

Start a session in the background and wait for a window:

```sh
waypuppet run --detach -- gtk3-widget-factory
waypuppet wait --windows 1
waypuppet ps
```

`run --detach` returns before the window is necessarily ready. `wait --windows 1` waits
until the session sees a window. `ps` lists the windows and their IDs.

If the window does not appear, run `waypuppet logs` to inspect the application's output.
The default wait timeout is 5 seconds. Set a different timeout in milliseconds with, for
example, `waypuppet wait --windows 1 --timeout 10000`.

## Capture the window

```sh
waypuppet shot
```

The command prints the path to a PNG file. Open that file to inspect the image. To choose
an output path, use `waypuppet shot -o PATH`. The path must be accessible to the session;
the default capture directory is shared with the caller.

## Inspect and use a widget

Print the accessibility tree and the selectors for its widgets:

```sh
waypuppet tree --selectors
```

The output depends on the application and toolkit. Copy a selector for a visible button,
then check it with `select`. For example, if the tree includes a button named “Save”:

```sh
waypuppet select 'button[name="Save"]'
waypuppet click --on 'button[name="Save"]'
waypuppet shot
```

`select` reports the match without clicking it. The selector must match a widget in your
application; the example name may not exist in the GTK demo. If a selector has several
matches, use `--index N` to choose one by its zero-based position in tree order.

You can also click a coordinate and send keyboard input:

```sh
waypuppet click --at 450,300
waypuppet type 'Hello'
waypuppet key Return
```

Choose coordinates for a text field in your window before you run these commands. To focus
an accessible widget and type in one command, use
`waypuppet type 'Hello' --focus-on 'entry[name="Search"]'` with a selector from your tree.
The application must publish an accessibility tree for selector commands to work.

Input commands wait for a frame after sending input. An input that does not cause a redraw
can reach the settle timeout. For example, moving the pointer over an unchanged area may
produce no frame.

## Stop the session

```sh
waypuppet kill
```

A detached session stays active until you stop it. Use `waypuppet sessions` to list active
sessions. To run more than one session, pass `--session NAME` to `run` and to subsequent
commands for that session.

For more commands, run `waypuppet --help`. For CI and image comparison, see the
[CI guide](ci.md).
