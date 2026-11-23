-- Boop integration for Rex (https://rex.sh)
-- Load from ~/.config/rex/config.lua with:
--   dofile("/path/to/boop/contrib/rex/boop.lua")
--
-- Provides 4 native Rex Command Palette actions synced across Mac and iOS Rex apps:
--   1. Boop: Publish OTA Build…     (boop_publish)
--   2. Boop: Build Inbox            (boop_builds)
--   3. Boop: New Agent Worktree…    (boop_new_worktree)
--   4. Boop: Pool Status & Stop     (boop_pool)

local function shell_quote(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function read_cmd(cmd)
  local h = io.popen(cmd .. " 2>/dev/null", "r")
  if not h then return "" end
  local out = h:read("*a") or ""
  h:close()
  return out
end

local function trim(s)
  return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function terminal_pwd()
  local ok, res = pcall(rex.block.call, "com.superlogical.terminal", "pwd", {})
  if ok and type(res) == "table" and res.pwd and res.pwd ~= "" then
    return res.pwd
  end
  return nil
end

local function write_terminal(cmd)
  rex.block.call("com.superlogical.terminal", "write", { data = cmd .. "\r" })
end

-- 1. Boop: Publish OTA Build… (Firebase App Distribution Build Inbox)
rex.action{
  name  = "boop_publish",
  title = "Boop: Publish OTA Build…",
  args  = { name = "string?", notes = "string?" },
  run   = function(ctx, args)
    local cwd = terminal_pwd()
    local default_branch = ""
    if cwd then
      default_branch = trim(read_cmd("git -C " .. shell_quote(cwd) .. " rev-parse --abbrev-ref HEAD"))
    end

    local name = args.name or rex.ui.input{
      title       = "Boop build name",
      placeholder = "New keyboard animation",
      value       = default_branch,
      confirm     = "Next",
    }
    if name == nil or trim(name) == "" then return end

    local notes = args.notes or rex.ui.input{
      title       = "Release notes (optional)",
      placeholder = "What changed and what to test on iPhone",
      confirm     = "Publish OTA",
    }
    if notes == nil then return end

    local cmd = "boop publish --name " .. shell_quote(trim(name))
    if trim(notes) ~= "" then
      cmd = cmd .. " --notes " .. shell_quote(trim(notes))
    end
    write_terminal(cmd)
  end,
}

-- 2. Boop: Build Inbox (pick any recent published build and open its Firebase testing URL)
rex.action{
  name  = "boop_builds",
  title = "Boop: Build Inbox",
  run   = function()
    local raw = read_cmd(
      "python3 -c 'import json,subprocess;"
      .. "r=json.loads(subprocess.check_output([\"boop\",\"builds\",\"--json\"]));"
      .. "for b in r.get(\"builds\",[])[:30]:"
      .. "  ver=(b.get(\"display_version\") or \"\")+\" (\"+(b.get(\"build_version\") or \"-\")+\")\";"
      .. "  print(\"\\t\".join([b.get(\"testing_uri\") or b.get(\"id\",\"\"), b.get(\"name\",\"Build\"), (b.get(\"project\",\"\")+\" · \"+(b.get(\"branch\") or \"local\")), ver]))'"
    )
    local items = {}
    for line in raw:gmatch("[^\r\n]+") do
      local id, title, subtitle, detail = line:match("([^\t]*)\t([^\t]*)\t([^\t]*)\t(.*)")
      if id and id ~= "" then
        table.insert(items, {
          id       = id,
          title    = title,
          subtitle = subtitle,
          detail   = detail,
          icon     = "package",
        })
      end
    end
    if #items == 0 then
      rex.log("info", "boop_builds: no builds recorded yet")
      return
    end

    local choice = rex.ui.pick{
      title       = "Boop Build Inbox",
      placeholder = "Filter builds",
      confirm     = "Open Testing URL",
      items       = items,
    }
    if choice == nil then return end

    if choice:match("^https?://") then
      os.execute("open " .. shell_quote(choice) .. " >/dev/null 2>&1 &")
    else
      rex.log("info", "Selected local-only build: " .. choice)
    end
  end,
}

-- 3. Boop: New Agent Worktree… (spin up isolated git worktree + Rex split ready for boop)
rex.action{
  name  = "boop_new_worktree",
  title = "Boop: New Agent Worktree…",
  args  = { slug = "string?", agent = "string?" },
  run   = function(ctx, args)
    local cwd = terminal_pwd()
    if not cwd then
      rex.log("warn", "boop_new_worktree: focus a terminal inside a git repo first")
      return
    end
    local repo = trim(read_cmd("git -C " .. shell_quote(cwd) .. " rev-parse --show-toplevel"))
    if repo == "" then
      rex.log("warn", "boop_new_worktree: current terminal is not inside a git repo")
      return
    end

    local slug = args.slug or rex.ui.input{
      title       = "Worktree task slug",
      placeholder = "keyboard-spring",
      confirm     = "Next",
    }
    if slug == nil or trim(slug) == "" then return end
    slug = trim(slug):gsub("[^%w%-_]", "-")

    local agent = args.agent or rex.ui.pick{
      title = "Launch in worktree",
      items = {
        { id = "claude", title = "Claude Code", subtitle = "claude", icon = "code" },
        { id = "codex",  title = "Codex CLI",   subtitle = "codex",  icon = "code" },
        { id = "shell",  title = "Plain Shell", subtitle = "zsh",    icon = "terminal" },
      },
    }
    if agent == nil then return end

    local wt = repo .. "-wt-" .. slug
    local branch = "feat/" .. slug
    local setup = table.concat({
      "git -C " .. shell_quote(repo) .. " worktree add -b " .. shell_quote(branch) .. " " .. shell_quote(wt) .. " HEAD",
      "[ -f " .. shell_quote(repo .. "/.boop.toml") .. " ] && cp " .. shell_quote(repo .. "/.boop.toml") .. " " .. shell_quote(wt .. "/.boop.toml") .. " || true",
      "[ -f " .. shell_quote(wt .. "/project.yml") .. " ] && (cd " .. shell_quote(wt) .. " && xcodegen generate) || true",
    }, " && ")
    os.execute(setup .. " >/dev/null 2>&1")

    local split, err = rex.session.new_split{
      layout = rex.layout.block{
        flavor  = "com.superlogical.terminal.shell",
        options = { cwd = wt },
      },
    }
    if err ~= nil then
      rex.log("warn", "boop_new_worktree: " .. tostring(err))
      return
    end
    if split and split.block_id then
      rex.session.set_block_label{ block_id = split.block_id, label = "boop:" .. slug }
      if agent == "claude" or agent == "codex" then
        rex.block.call("com.superlogical.terminal", "write", {
          block_id = split.block_id,
          data     = agent .. "\r",
        })
      end
    end
  end,
}

-- 4. Boop: Pool Status & Stop (inspect forks or pause all Boat compute billing)
rex.action{
  name  = "boop_pool",
  title = "Boop: Pool Status & Stop",
  run   = function()
    local choice = rex.ui.pick{
      title = "Boop Worker Controls",
      items = {
        { id = "status",  title = "Show pool & worker status", subtitle = "boop pool status", icon = "server" },
        { id = "stop",    title = "Stop workers ($0/hr)",      subtitle = "boop stop",        icon = "clock" },
        { id = "release", title = "Release current worktree fork", subtitle = "boop release", icon = "branch" },
        { id = "doctor",  title = "Run health check",          subtitle = "boop doctor",      icon = "gear" },
      },
    }
    if choice == nil then return end

    if choice == "status" then
      write_terminal("boop pool status || boop status")
    elseif choice == "stop" then
      write_terminal("boop stop")
    elseif choice == "release" then
      write_terminal("boop release")
    elseif choice == "doctor" then
      write_terminal("boop doctor")
    end
  end,
}
