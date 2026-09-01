---
title: Luvus Is Built for Composable Mission Control
description: Luvus connects panes, agents, worktrees, modules, and automation through one living system rather than a pile of disconnected features.
date: 2026-08-24
author: Riz
hero: /blog/luvus-built-for-composable-mission-control.png
heroAlt: A whiteboard diagram connecting many small modules to one central workspace
heroWidth: 1672
heroHeight: 941
---

Most terminal tools grow by accumulation, first come panes, then tabs, then a sidebar, then Git, then agent status, then worktrees and then automation. Before long, every feature knows just enough about every other feature to make the product useful and the codebase difficult to extend.

This is where Luvus takes a different route. It treats the terminal as a shared workspace for work that is already happening. Panes, tabs, coding agents, Git state, worktrees, tasks, modules, and remote clients are not separate mini applications stitched together by shortcuts. They are different views of the same running system.

A Luvus server owns the living parts of the workspace: terminal processes, layouts, scrollback, agent identity, session state, and persistent project structure. Clients attach to that server and render what it knows. The command line, the interface, and local automation all speak to the same source of truth. So when a pane moves to another tab, an agent is renamed, a worktree is opened, or a task is assigned, it is not a UI trick. It is a real operation on the workspace itself.

## One workspace, several ways in

Good terminal software should not hide its best features behind one interface. If you can create a pane from the UI, you should be able to create it from the CLI. If the sidebar can inspect an agent, a script should be able to inspect that same agent. If a module can show a dock, it should be able to work with the same panes, tabs, and tasks you already have open.

That is the practical side of composability. The UI is there for people, the CLI is there when you want to move quickly, and the local API is there when a script or another tool needs to join the conversation. They all operate on the same workspace instead of maintaining their own version of it.

## Extensions that do not feel bolted on

Luvus modules are plain directories with a `luvus-module.toml` file and commands that run on your machine. A module can be a shell script, a Python tool, a Rust binary, or anything else executable. There is no small custom language to learn just to add a useful feature.

The manifest lets a module add actions, event hooks, panes, sidebar docks, bar widgets, settings, and startup work. Luvus takes care of the interface and lifecycle while the module handles the actual job. That makes a module feel like part of the app without forcing its author to build a settings page, a context menu, or a background service from scratch.

The important detail is that modules are not decorative. A right click action can use the text you selected in a pane. A dock can show task state. A small bar widget can report progress without taking over the screen. The extension can act on the same workspace that it is describing.

## What coding agents change

Coding agents make terminal work less linear. A command used to begin, print its result, and disappear into scrollback. An agent has a longer life. It may be working in a separate worktree, waiting for permission, asking another agent a question, or returning to a session after the app restarts.

That changes the design problem. It is no longer enough to launch a process and draw its output. A useful workspace needs to remember the relationship between a terminal, an agent, its session, the repository it is changing, and the task that led to it. If that relationship disappears, the user is left reconstructing context from tabs and terminal history.

It also changes what good automation looks like. Reading a screenshot or polling a terminal for a phrase is fragile. A better system can say that an agent is waiting, that a pane has moved, or that a process was replaced. Those are state changes, not visual guesses. A versioned protocol is simply a way to make those facts available without binding every integration to the internals of the app.

The broader lesson is not that every terminal needs every feature. It is that features become easier to live with when they share a model of the work. A pane knows which workspace it belongs to. A task can point to an agent. A module can work with the same state the interface displays. The pieces stay small, but they stop feeling disconnected.

That is the kind of composability worth aiming for. Not a catalogue of integrations, but a system where the next useful idea has somewhere sensible to attach.

## Keep exploring

- [Control Luvus with the local API](/docs/uhp/methods/)
- [Build a module in any language](/docs/extend/writing-modules/)
- [Add compact status with Luvus Bar](/docs/guides/bar/)
