# Project Arsox

Run a fleet of docker based, self-hosted Claude/Codex satellite workers that can ephemerally work through a job queue of given tasks and be managed through a controlled SDK channel.

Using PolyMorphism we support Claude, Codex, and other LLMs as the workers and they can work through managed tasks from a controlling application.

We provide the SDK and the satellites, your application manages what they do. Your application sends job requests to work on x/y/z tasks, settings and LLM keys. The satellite handles the job, streams the results back in realtime through a standardized message shape, and provides a robust API + SDK to make it extremely easy to get and manage the statuses.

Satellites naturally come with a job queues, and support threading multiple contexts of conversations together to resume conversation threads. It's literally like using Claude with a terminal as a human, but fully as an SDK and supporting fully remote architecture.

## How you use it

These are hosted for free on docker.io as the image:
> jalapenolabs/ArsoxSatellite-ubuntu:latest

```Dockerfile
FROM JalapenoLabs/ArsoxSatellite-ubuntu:latest
```

There are also variants for:
- `JalapenoLabs/ArsoxSatellite-fedora:latest`
- `JalapenoLabs/ArsoxSatellite-rocky:latest`

<!-- TODO: Show a docker compose example -->

Optionally, you can build them yourself too.

Importantly, you will need to configure a security key via ENV.
```conf
ARSOX_SECRET=123
```

This will be used with your host application's API to authenticate it, so this API can be exposed onto the internet but calls will not be responded to without it.

Omitting this value will allow anyone who can ping the satellite's API to be able to command it, which may lead to security vulnerabilities.

<!-- TODO: Enter code details about how to configure it -->
<!-- TODO: Show SDK examples of how to use it -->

The SDK is available in x3 languages:
- Rust (via Cargo) <!-- TODO: Put link here when it's available -->
- Node/Web (via NPM) <!-- TODO: Put link here when it's available -->
- Python (via PyPi) <!-- TODO: Put link here when it's available -->

For setting up the Satellite's workspace, you are typically expected to use this in Dockerfile to extend the image and install your own stuff on top of it. For example, if you need golang on this then you should pull this `FROM` on dockerfile and use the `RUN` command to install it yourself.

## Architecture

The satellite spawns with a Rust API running on it.
The SDK already has each API route registered into it, so you just need to call the SDK at the given endpoint and it'll make the API requests for you.

The API itself uses Protobuf mainly first, this helps enforce a strong request/response shape type and forces a universal shape bus. This is one of the major benefits of using Satellites, regardless of if you prefer the Codex CLI harness or the Claude CLI harness-- You can use either but still always get the exact same, strongly typed shape out of each.

There is a secondary JSON API, and you can forcibly use JSON by commanding the SDK to use it OR by passing `Content-Type` appropriately as `` <!--TODO: Put proper JSON header value here -->.

The secret is passed via the `Authorization` header in all of the requests from the SDK.

The satellite can be queried for it's current state or it can be commanded upon.
There's also an SSE channel that opens with the SDK, which will emit messages 

The satellite will handle:
- The LLM polymorphism (converting Claude request/response shapes to be used by Codex, for example).
- Job queues
- Settings/config
- Saving/clearing workspaces between jobs (optional) and workspace management

The docker container provides lots of packages out of the gate that help Claude/Codex perform faster. These include:
- git
- build-essential
<!-- TODO: Populate! -->

Several CLIs are also pre-installed, such as:
- gh
- jira

### The workspace

On the satellite's volume itself, the workspace is mounted to:
> /workspace

Inside of it, it may look a little like this:
```
/workspace
  |-- /conversation-thread-id-1
    CLAUDE.md
    CODEX.md
    AGENTS.md
    |-- .claude/
    |-- .codex/
    |-- /repos
      |-- /repo-name-1
      |-- /repo-name-2
      |-- /repo-name-3
    |-- /artifacts
  |-- /conversation-thread-id-2
    CLAUDE.md
    CODEX.md
    AGENTS.md
    |-- .claude/
    |-- .codex/
    |-- /repos
      |-- /repo-name-1
      |-- /repo-name-2
      |-- /repo-name-3
    |-- /artifacts
```

Resuming threads is fully available to you to configure, and you can also have them teardown afterwards too.

The agent files such as CLAUDE.md, CODEX.md, are all pointers towards AGENTS.md. They will point to it with `@/workspace/<thread-id>/AGENTS.md` and this is how we can enable global custom instructions onto the runner.

Repos are not required in order to perform any work.

### HTTP requests

The SDK typically handles this all for you, so you won't need to worry about this.
The SDK always uses protobuf and always converts it from protobuf into code-ready, usable objects for you in your code (For example, it'll be a Typescript standard JSON object if using the NPM SDK).

For text reports and markdown reports, the SDK also provides methods for you to get these if needed. JSON is never used in the SDK.

This section really only applies if you're making HTTP requests manually (via Postman, for example).

You must use the `Authorization` header that matches the env ARSOX_SECRET environment value in order to access the service.

Additionally, you can pass one of x3 values for the `Content-Type` header:
- `application/protobuf` (default)
- `application/json`
- `text/plain`
- `text/markdown`

When requesting from text/plain or text/markdown, you'll get UTF-8 human-readable information.
The API will generate text-based reports for each of these.

The SDK can be used to run multiple conversation threads at the same time, which each have their own commander + team (unless team mode is off).

### State management

Internally, the API that orchestrates the Satellites has a state. 
Arsox ships with it's own sql-like database that runs with it in the API.
Database files are stored in `/var/arsox/database`.

#### Lifetime statistics

Lifetime statistics will keep track of all statistics.
Things like, lifetime tokens used (total, input, output) per LLM model.
Total tokens used for each thread (total, input, output).

In a human readable format via HTTP request:
```
Model: Opus 4.8 [1m]
1,234,567 total tokens
1,234,567 input tokens
1,234,567 output tokens

<Thread id 1>
1,234,567 total tokens
1,234,567 input tokens
1,234,567 output tokens
```

Statistic events will NOT be emitted down the SSE socket when it changes, unless the SDK explicitly opts-in in stream settings, as it may change very rapidly.
This can be received with a GET request through the SDK.

## Settings

There's many settings available to help you on your journey.
These are passed from the SDK and they let you take full control of the satellite.

### Repo settings

If you have a git repo that you'd like to leverage, this is how you can do it.

First, we need a repo endpoint. This could look like:
> https://github.com/JalapenoLabs/arsox-satellites.git
> git@github.com:JalapenoLabs/arsox-satellites.git

The satellite will attempt to clone it with submodules.
```bash
git clone --recurse-submodules
```

This works well if your repository and submodules are all public. If they're private, you will need to provide authentication.

We will expect you to provide one of these from the SDK:
- SSH private/public key pairs
- A PAT token to clone it from
<!-- TODO: Revisit if more auth forms are supported -->

We also allow you to provide repo set up commands (commands such as `yarn install` or `pip3 install`).
This can be a multi-line string, and you can use semi-colons or newlines to separate each command out.

Multiple repos can be provided if desired.

Repos can also be configured with a checker command.
This can be a multi-line string, and you can use semi-colons or newlines to separate each command out.
Commands separated by newlines are done in parallel with each other, and do NOT fail fast.
Commands separated by semi-colons do fail fast and block commands below them.

The checker command will run after claude/codex determines that it's work is fully complete.
You can then run verification on it, such as `yarn lint` or `npm run typecheck` and if it doesn't exit as code 0 then it will re-awaken the claude/codex instances to fix it or ignore it before exiting fully.

This is a nice way to help add extra verification to your LLM's work before allowing it to return to your SDK as completed. If a checker is provided, the SDK will include it in it's report.

An example checker:
```
yarn install;
yarn lint
yarn typecheck
yarn generate && yarn build
; yarn deploy --dry-run
```

In this above example, yarn install will run first and nothing else will run until it finishes because the semi-colon dictates that it must be awaited.
Then, the lint + typecheck + generate/build jobs will be ran in parallel. If typecheck fails it won't block the others (no failing fast).
yarn deploy won't fire until all of the steps before it have successfully completed. This means that if typecheck failed, deploy will never run (intentionally).
These logs are captured and sent to the LLM automatically.

### Github

You can pass a PAT into the settings, which will allow the agents to be able to use Github.
This is highly recommended, and will enable the agent to run any needed GH CLI command.

When creating the PAT, we recommend allowing:
- <!-- TODO: Populate for PRs, reading GH actions, reading vars/secrets -->

Possible settings:
- Allow/disallow merging of PRs (defaults to allowed by default).
- <!-- TODO: Populate -->

Today, Github will be supported first-class.
We will develop this with polymorphism, as to easily allow other providers (Gitlab and Bitbucket) to be used in the future.

### Jira

You can pass a Jira PAT into the settings also, which will allow the agent to lookup items.

Possible settings:
- Allow/disallow moving ticket statuses (defaults to allowed)
- Allow/disallow commenting on tickets (defaults to allowed)
- <!-- TODO: Populate -->

### Custom remote ENV

You can pass an array of objects of custom env variables to have Claude leverage.
```json
[
  { "key": "", "value": "", "isSecret": true }
]
```

Only `key` and `value` are required.
If isSecret is not provided, then it will default to `true` for security purposes.
Every secret always gets scanned for in the data before it's sent back to the SDK sender, and will always be hidden.

### Ephemeral / cleanup

Whenever you spawn a new thread / session, you are required to specify how long it should exist on disk for.

The lifetime value is measured in minutes, and will clean up it's workspace after N minutes. This is always required, as a safety net.

You can also have it immediately delete once the thread calls are completed.

### LLM to use

You can pick whether to use the Claude CLI harness or the Codex CLI harness. By default, we use Claude.
This is interesting, you could use Anthropic Claude models such as `Opus 5 [1m]` with the Codex CLI.

As we expand, we plan to also add support for other LLMs such as (but not limited to):
- Deepseek
- Bedrock
- <!-- TODO: Add others here! -->

We are using a LiteLLM sidecar service to help transform request/response LLM shapes for us, to match the required standard shapes for each service. The LiteLLM service will transform an inbound Deepseek conversation request into a Codex-compatible response.

LiteLLM on it's own is a HUGE ecosystem, it ships with a database, API, and more. However, there's a much lighter version of it that ships only the message request/response transforming capabilities that we are seeking. We use this lighter method for supporting a significantly larger array of LLM models. Anything that LiteLLM supports, we can support too.

We support Anthropic auth tokens (subscription access/refresh tokens, `claude setup-token` tokens, and API keys) and OpenAI auth tokens (subscription access/refresh tokens and API keys).

We also support using custom LLM endpoints, for example self-hosting Azure Anthropic models.

Additionally, you can pass multiple LLM endpoints into each job. If one endpoint errors (maybe usage is filled up, or maybe OpenAI is hitting a 529), then it will switch to the next. The order matters, it will proceed from one to the next. This will allow you to stack multiple subscriptions on top of each other or multiple API keys. You can also specify the same LLM endpoint + key but a different model, for example.

When defining models, you can also configure it's retry count. By default it will retry up to 10 times with a increasing timeout/wait between them, starting with a 5 second delay and up to a 60 second delay between each. In case you want it to retry up to N times on a 529 or 429 rate limit error code. You can also specify exact error codes to retry on, other than the standard 529/429 codes.

### Permissions

You can pass along:
- Web permissions (all / none / preset / custom domain list) (defaults to preset).
- Blacklisted commit/push branches (you can blacklist pushing commits directly to `main` for example).
- Whitelist of exec commands that these CLIs can run (defaults to allow all).
- Allow/disallow pushing commits (defaults to allowed).
- <!-- TODO: Add more here once added! -->

### Prompt

You can provide a global prompt which will be populated into `/workspace/<thread-id>/AGENTS.md`.
CLAUDE.md and CODEX.md will automatically be created for the harness and point at this universal agents file, to fully ensure that it's always loaded.
There may be prompts placed into the header of that AGENTS.md file from the Arsox system, but then your own details will get populated below.

### Secret exposure logging

When logs, conversation, and data is streamed back to the remote viewer via SDK then we ensure no secrets are ever leaked in the logs. This is data from the env marked as `isSecret`.

You have 3 options for how secrets are securely trimmed out of logs:
- Anonymous (`sk_ant_12345` becomes exactly x6 stars `******`)
- PrefixShown (`sk_ant_12345` reveals the first N chars with exactly x6 stars, becomes `sk_ant_123_******`, where N is configurable and is default 8 or 20%, whichever is lower)
- PostfixShown (`sk_ant_12345` reveals the last N chars with exactly x6 stars, becomes `******nt_12345`, where N is configurable and is default 8 or 20%, whichever is lower)
- HybridShown (`sk_ant_12345` reveals the first and last N chars with exactly x6 stars, becomes `sk_an******2345` where N is configurable is default 5 or 10%, whichever is lower.)

Additionally, instead of exactly 6 stars you can configure it to show a different exact number of stars OR honor it's length by setting it to less than 0 (`-1`). When set to 0, it will always show at least 1 star.

### Streaming settings

You can opt-out of what data you receive. By default, you'll receive all events and data streamed via SSE.

You can toggle off specific events, by default you opt-in to all events.

There is always **ONE** SSE socket per Satellite. Never more, never less.

It doesn't matter if you're using Claude CLI or CodexCLI , you'll always get the same shape of data.
If using the Claude CLI, for example, and it emits a `tool call started` event, then you'll get a standardized shape via the SDK. If you switch to the Codex CLI, which emits the same kind of event but a slightly different shape, then it'll be conformed to the same standardized shape via the SDK.

## Team mode

By default, Satellites use a "team mode." You can opt out of this if you wish.

Opting out will feel a lot more like a standard Claude/Codex session, where you talk to a single agent and a single context limit for it. It's role will be "agent" in this mode.

While in team mode, there will be a root "commander" LLM which is responsible for seeing the entire task through it's completion. It's first task on each turn will be to designate what other team members it will need. This does NOT mean the built-in sub-agents that it has, instead it will designate other LLM team members to use.

For example, the commander may choose to spawn a team with (but not limited to):
- Architect team member
- Backend team member
- Frontend team member
- CI team member
- QA team member
- Unit test team member
- Doc writer team member

These team members will be able to communicate with each other (a team chat and a DM chat).
Arsox will provide all of the infrastructure necessary for them to easily communicate with each other and know the assignments.

These team members can then spawn their own sub-agents natively.
This creates a 3 tiered tree of agents.

This drives 3 very valuable features:
1. Max parallelizm.
Code such as backend + frontend + unit tests can all be created at nearly the same time.
They can move quickly and coordinate.
2. Max efficiency of context. 
If we only get an average of 1 million context per LLM agent, then why have one agent do both frontend + backend? There's strong value in the full ownership of context. Having a frontend agent primarily dominate frontend files, and having a backend agent primarily dominate backend files... Now we have 2m total context and each context lane's quality is significantly more optimized per-role.
3. Quality of ownership.
When team members take ownership of certain aspects of the application, they can be encouraged to do better and perform better best-industry practices. It also encourages collaboration and micro-decisions made by team discussion encourages better product direction + decisions. A team can debate things, work through issues, generate ideas. A team will always beat the individual.

Competition is also encouraged.
> The team members play against each other, and the commander plays against the other commanders.

The commander doesn't really have access to other commanders, but it must imagine that it is. It must have the mindset that it's team must be as successful as possible.
Speed is encouraged over quality. It could take several hours or even days to get something done, that is 100% fine.
The competition is to have the highest quality output and refactor/refine their work to be the best. The most thoroghly tested, thought through, well documented, and most production-ready.

The commander LLM will watch the team chat and help ensure everyone stays on track. The commander can always see all DMs, if the frontend team member messages the backend team member directly then the commander sees and tracks it.
At the very end of the session, the commander determines what files are an artifact, and typically the commander responds back to the conversation via the SDK directly.
The SDK captures all team member's logs (activity, discussions, tool calls, etc).
The commander also ensures fair competition and assigns/retracts points (1, 2, or 3) as team members get things done.

This is configurable, you could drive a limit of team members that it can spawn.
By default, it will not spawn more than 8 team members.
By default the commander LLM chooses the roles (not from a bucket, but from a suggestion list).
You can also provide your own suggested roles to ADD to the suggestion list.

Notably, the team always spawns together and despawns together.
Additionally, the commander may spawn or despawn team members through the run, it is not limited to what it started with.
It can do this with MCP that's provided to the commander via Arsox.

## Human in the loop

By default, the human is in the loop. You can disable this if you'd like, and let the team fully run the entire task.
The LLM can ask for approvals (Do you approve this command / approach) and it can ask for clarification questions.

For clarification questions, it'll behave just like how Claude Code CLI behaves. It can ask up to 5 questions at a time (minimum 1) in one request. One set of questions per thread, it should NEVER allow a scenario such as x10 queued questions, where there are x2 queued question sets at the same time.

The question can have a title, up to x5 choosable options (each with a title and sub-description).

The SDK must respond to all questions in one response, not partially.
If x5 questions are asked, the SDK must respond with either answering all x5 in the response message or respond by declining all x5 altogether.
That being said, each response can be freeform or it can partially declined through individual responses.
The SDK can respond to questions 1-4 by giving it back it's own options, or a freeform string response. Then perhaps question 5 can be marked as declined to answer.
However, the response shape must be given all x5 at the same time when sent.

This can all be disabled by the SDK, and you can make the LLM use it's own best judgement to complete the task.
This can be a little dangerous but is up to the SDK's implementer.

## Built in skills

Arsox satellites will come with built-in skills that can be used.
They will be provided in each conversation thread as `/workspace/thread-id/.claude/skills` or `/workspace/thread-id/.codex/skills`

## Plan mode

EXPERIMENTAL. This is an opt-in feature, default off.

What this will do, is first draft up a plan for review. Even with team mode, this will use a dedicated LLM agent with a clean context window just for the plan mode execution.
This will have the agent run, and first generate a full plan for the entry request. Claude and Codex CLIs both have plan modes, but if unavailable to the CLI then they will use a skill fallback that's included with Arsox.
The plan will be sent back to the SDK for review. Once approved, it will proceed.
If human in the loop is turned off OR if the user turns on auto-approving the plan mode, then it will assume it's approved and proceed like normal to the commander (or to the agent if team mode is disabled).

## Automated self-review

EXPERIMENTAL. This is an opt-in feature, default off.

Arsox will provide a default skill for reviewing it's own code.
It will spawn a dedicated LLM agent with a clean context window to scan all of it's changes and review itself.
If attached to a pull request, then it will use the pull request as the primary medium for leaving review comments as it reviews it's own work.
If not attached to a pull request (perhaps a commit already made, or it's just generating an artifact) then it will leave the full review into a file for the Claude agents to review.
It could suggest changes to the artifact before it's uploaded, or suggest follow up commits onto a branch to improve something that wasn't done well enough.
If there's no file outputs (nothing to review) then this part will be skipped.

## Pull request merging

EXPERIMENTAL. This is an opt-in feature, default off.

By default, we'll require a human to merge pull requests on their own.
If you want more autonomy given to the LLM, you can allow the commander to merge the pull request on it's own after a task is fully completed + reviewed + everything.

The commander will **choose** to merge it or not.
You can also define policy for which PR merging methods to allow (such as squash/merge vs rebase).

## Artifacts

When a job is completed, it will have artifacts ready/available to you.
For example, maybe it generated a text report that wasn't commited/pushed to a git repo.

These files will be available until the thread expires.
If the thread is ephemeral, the SDK provides a method to always download/absorb the artifacts before it fully completes.

The LLM will determine what is an artifact to include and what isn't.
You can also use the SDK to list all files (you can list all non artifacts and list all artifacts), and you can download/upload any files to it to/from your host application.

Artifacts are moved upwards from the repo level to the artifacts folder in the conversation dir, as repo dirs are more likely to be torn down or re-created (the most ephemeral) but artifacts can persist in the thread longer-term.

## MCP

Satellites also support MCP! So you could integrate it with your Claude/Codex sessions and it can use them to leverage outbound tasks.

## Order of operations

There is a "stack" of operation orders that occur on the satellite, in order to fully complete a turn.

Here's the stack, in order of operation for a new turn on a new thread:
1. Create a thread (defines repos and settings) (SDK call)
2. A turn starts (SDK call)
3. If plan mode is enabled, a plan mode agent begins working through a plan and awaits review. The agent could also determine if a plan is not needed and allows skipping this step.
4. A commander is spawned and ingests the job, and spawns a team
5. The team works until they are all completed
6. The team despawns
7. Automated checkers run, and if any fail then it brings it back to the commander's attention to re-assign to their team. Note: Checkers could be failing and skipped by the LLMs intentionally, so it should be skippable if the commander determines it so. If the commander chooses to skip it, it should not "stay skipped" for future turns.
8. Automated self-review triggers here, if enabled.
9. Auto squash/merge triggers here, if enabled.
10. Artifact scanning from the commander is done here
11. Artifact uploading to the SDK triggers here if the SDK wants it back immediately/automatically.

Here's the stack, in order of operation, if the thread is already created and still open (re-using an existing thread):
1. A turn starts (SDK call)
2. A new plan agent is created with new context, and analyzes the turn + history. It will assess if plan mode is needed. If so, it'll then run a new plan creation.
3. The commander is spawned again but with all of it's previous context still in tact. It spawns it's team. Critically: The commander can choose if team members spawn with a clean slate (no context) or with their existing previous context.
4. The team works until they are all completed.
6. The team despawns.
7. Automated checkers run, same as new thread.
8. Automated self-review triggers here, if enabled, same as new thread.
9. Auto squash/merge triggers here, if enabled, same as new thread.
10. Artifact scanning from the commander is done here, same as new thread.
11. Artifact uploading to the SDK triggers here if the SDK wants it back immediately/automatically, same as new thread.

The SDK can destroy threads itself, or let them expire ephemerally.
