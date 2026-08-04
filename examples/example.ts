// Copyright © 2026 Jalapeno Labs

/**
 * End to end example of driving an Arsox satellite from Node.
 *
 * Creates a thread with the full settings surface, runs a turn, consumes the
 * normalized event stream, answers the agent's questions, and collects artifacts.
 *
 * Run with: tsx examples/example.ts
 */

import type { QuestionAnswer, Thread, ThreadSettings } from '@arsox/sdk'

// Core
import { Satellite } from '@arsox/sdk'

// Misc
import {
  Harness,
  MergeMethod,
  PrefetchInjection,
  RedactionMode,
  ServiceIsolation,
  Viewport,
  WatchTrigger,
  WebAccess
} from '@arsox/sdk'


// Environment variables cross a runtime boundary, so they get a real check.
const arsoxSecret = process.env.ARSOX_SECRET
if (!arsoxSecret) {
  console.error('ARSOX_SECRET is not set, refusing to start')
  process.exit(1)
}

const satellite = new Satellite({
  url: 'https://satellite-01.internal.jalapenolabs.io',
  secret: arsoxSecret
})

// Stand-in for your own durable sink: a table, a message bus, a log file.
const auditLog = {
  write(entry: unknown): void {
    console.debug(JSON.stringify(entry))
  }
}

// Stand-ins for your own issue tracker. `alreadyFiled` holds the suggestion
// fingerprints you have seen before, which is what stops the same finding
// becoming a new ticket on every turn.
const alreadyFiled = new Set<string>()

async function openIssue(title: string, body: string): Promise<void> {
  console.log(`would file: ${title}\n${body}`)
}


// ///////////////////////////// //
//         Thread settings       //
// ///////////////////////////// //

const settings: ThreadSettings = {
  // The satellite collects the workspace after this much inactivity. The clock
  // resets on every turn, so a thread working for three days is never collected.
  // Required, always, as the safety net against forgotten workspaces.
  idleTtlMinutes: 120,
  deleteOnComplete: false,

  // Required. `unlimited` is accepted but has to be typed out, so an unbounded
  // spend is always a decision rather than an oversight.
  budget: {
    maxTokensPerTurn: 8_000_000,
    maxCostPerThread: 40.0,
    maxWallClockPerTurn: 'unlimited'
  },

  harness: Harness.Claude,

  // Your own agent configuration: CLAUDE.md, docs, custom skills. Cloned once
  // into .agents, then materialized into .claude and .codex. Pin `ref` to a tag
  // or a commit. A floating branch here changes agent behavior between two
  // threads you believed were identical.
  agentsRepo: {
    url: 'git@github.com:navarrotech/agents.git',
    ref: 'v2.4.0',
    auth: { sshPrivateKey: process.env.AGENTS_DEPLOY_KEY }
  },

  // Ordered failover. The satellite walks this list top to bottom on failure,
  // so put the cheapest and most reliable endpoint first: moving to the next
  // endpoint discards the cached prompt prefix and the next request pays full
  // price for the whole history.
  models: [
    {
      name: 'primary-subscription',
      model: 'claude-opus-5[1m]',
      auth: { subscriptionToken: process.env.ANTHROPIC_OAUTH_TOKEN },
      retry: {
        maxAttempts: 10,
        initialBackoffSeconds: 5,
        maxBackoffSeconds: 60,
        retryOnStatus: [ 429, 529 ]
      }
    },
    {
      name: 'fallback-api-key',
      model: 'claude-sonnet-5',
      auth: { apiKey: process.env.ANTHROPIC_API_KEY },
      retry: { maxAttempts: 3 }
    },
    {
      name: 'self-hosted-azure',
      model: 'claude-opus-5',
      baseUrl: 'https://arsox-models.openai.azure.com/anthropic/v1',
      auth: { apiKey: process.env.AZURE_ANTHROPIC_KEY }
    }
  ],

  teamMode: {
    enabled: true,
    maxMembers: 6,
    // Added to the commander's suggestion list, not a fixed roster. The
    // commander still picks who it actually needs.
    suggestedRoles: [ 'Backend', 'Frontend', 'Unit test', 'Doc writer' ]
  },

  planMode: {
    enabled: true,
    autoApprove: false
  },

  humanInTheLoop: {
    enabled: true,
    // Past this, the turn ends with the questions recorded in the report
    // rather than hanging forever.
    questionTimeoutMinutes: 30
  },

  selfReview: {
    enabled: true
  },

  suggestions: {
    enabled: true,
    // Any real codebase yields fifty findings. Fifty per turn is noise that
    // teaches you to ignore the feature, so the cap forces ranking.
    maxSuggestionsPerCategory: 5
  },

  pullRequests: {
    allowAgentMerge: false,
    allowedMergeMethods: [ MergeMethod.Squash ]
  },

  // The satellite starts turns on its own here, which nothing else in Arsox
  // does. maxAttempts is the control that matters: fix, fail, fix, fail has no
  // floor without it.
  watchPullRequests: {
    enabled: true,
    maxAttempts: 3,
    watchWindowMinutes: 240,
    pollIntervalSeconds: 20,
    // Only react to check runs on commits the satellite itself pushed. `Any`
    // also reacts to human pushes, which is usually two parties editing the
    // same branch at cross purposes.
    reactTo: WatchTrigger.SatelliteCommits
  },

  repos: [
    {
      name: 'api',
      url: 'git@github.com:JalapenoLabs/arsox-satellites.git',
      baseBranch: 'develop',
      auth: {
        sshPrivateKey: process.env.DEPLOY_KEY,
        sshPublicKey: process.env.DEPLOY_KEY_PUB
      },
      // Semicolons are barriers, newlines run in parallel without failing fast.
      setupCommands: 'yarn install --immutable',
      checker: [
        'yarn install;',
        'yarn lint',
        'yarn typecheck',
        'yarn generate && yarn build',
        '; yarn deploy --dry-run'
      ].join('\n'),
      // Started once per thread, not once per member, and lazily on first use.
      // Every member gets ARSOX_SERVICE_WEB_URL rather than assuming a port,
      // which is what stops three agents racing to bind 3000.
      services: [
        {
          name: 'web',
          command: 'yarn dev',
          port: 3000,
          readyWhen: { httpGet: '/health', timeoutSeconds: 120 },
          isolation: ServiceIsolation.Shared
        }
      ]
    }
  ],

  github: {
    token: process.env.GITHUB_PAT,
    allowMerge: false
  },

  jira: {
    token: process.env.JIRA_PAT,
    baseUrl: 'https://jalapenolabs.atlassian.net',
    allowStatusTransitions: true,
    allowComments: true
  },

  // Fetched deterministically before the turn, off the model's clock. Jira
  // comes back raw so custom fields survive; a PR brings its diff, reviews,
  // conversation, and check status. Attachments from the item and from every
  // comment land alongside it.
  prefetch: {
    jira: [ 'BUG-123', 'PLAT-456' ],
    github: [ 11, 12, 13 ],
    // One line per item in AGENTS.md pointing at issues/<id>/. Summary inlines
    // every rendered issue, which is the token cost this feature exists to
    // avoid. Let the agent read what it needs.
    injection: PrefetchInjection.Index
  },

  // `isSecret` defaults to true when omitted, because defaulting to secret
  // fails safe. Spelling it out here for clarity.
  env: [
    { key: 'DEPLOY_TARGET', value: 'staging', isSecret: false },
    { key: 'DATABASE_URL', value: process.env.STAGING_DATABASE_URL, isSecret: true }
  ],

  redaction: {
    mode: RedactionMode.PostfixShown,
    // Six stars regardless of the secret's real length. Set to -1 to mirror the
    // length, which leaks the length and is why it is not the default.
    starCount: 6,
    // The kill switch. False unregisters the override_redaction tool entirely,
    // so no agent in this thread can reach it no matter what it is told.
    allowRedactionOverride: false
  },

  permissions: {
    // `web` picks the base list. Preset is the curated set the harnesses already
    // reach for (npmjs.org, pypi.org, crates.io, and friends). Custom starts
    // from nothing.
    web: WebAccess.Preset,
    // Always additive on top of `web`, so this never silently drops the preset.
    additionalDomains: [ 'docs.anthropic.com', 'jalapenolabs.atlassian.net' ],
    allowGitPush: true,
    protectedBranches: [ 'main', 'develop' ],
    // Omit to inherit the preset allowlist. An explicit list replaces it.
    allowedCommands: [ 'git', 'yarn', 'node', 'rg', 'gh' ]
  },

  // Written to /workspace/<thread-id>/AGENTS.md, below the Arsox header.
  // Advisory: it shapes behavior but never constrains it. Anything that must
  // hold belongs in `permissions` above.
  prompt: [
    'This repo is public and open source. The develop branch is the working branch.',
    'Never use em dashes in user-facing text.',
    'Update docs/ in the same change as the code.'
  ].join('\n'),

  mcpServers: [
    {
      name: 'internal-search',
      url: 'https://mcp.internal.jalapenolabs.io/sse',
      headers: { Authorization: `Bearer ${process.env.INTERNAL_MCP_TOKEN}` }
    }
  ],

  // Headless Chrome for members that need to see what they built. Each member
  // gets its own browser context, not its own process, and points at the
  // ARSOX_SERVICE_* addresses above. Traffic still goes through the egress
  // proxy: navigating to a URL is a network request wearing a hat.
  virtualBrowser: {
    enabled: true,
    allowedRoles: [ 'Frontend', 'QA' ],
    viewports: [ Viewport.Mobile, Viewport.Tablet, Viewport.Desktop ]
  },

  // Opt out of the noisy ones. Statistics are off by default because they
  // change on every token.
  stream: {
    includeStatistics: false,
    includeAgentThinking: true,
    includeToolCalls: true,
    includeTeamChat: true
  }
}


// ///////////////////////////// //
//   Style 1: the event emitter  //
// ///////////////////////////// //

/**
 * Registers push handlers on the thread.
 *
 * Events belong to the thread, not to a turn: one socket per thread, carrying
 * every turn that runs on it. Each event carries `turnId` if you need to
 * attribute it.
 *
 * Handlers fire concurrently and never block delivery, which is what makes this
 * the right default. A slow handler (a human answering a question, a database
 * write) holds up nothing behind it.
 */
function attachHandlers(thread: Thread): void {
  thread.on('agent.message', (event) => {
    console.log(`[${event.author}] ${event.text}`)
  })

  thread.on('tool.started', (event) => {
    console.log(`[${event.author}] ${event.toolName}`)
  })

  thread.on('team.member_spawned', (event) => {
    console.log(`+ ${event.role} (${event.memberId})`)
  })

  thread.on('team.chat', (event) => {
    console.log(`[team] ${event.author}: ${event.text}`)
  })

  thread.on('integration.landed', (event) => {
    console.log(`merged ${event.memberId} into ${event.branch}`)
  })

  thread.on('integration.conflict', (event) => {
    console.log(`conflict from ${event.memberId}, returned for resolution`)
  })

  thread.on('checker.result', (event) => {
    console.log(`checker ${event.command} exited ${event.exitCode}`)
  })

  thread.on('budget.warning', (event) => {
    console.warn(`budget at ${event.percentUsed}% of ${event.ceiling}`)
  })

  thread.on('artifact.created', (event) => {
    console.log(`artifact ${event.path} (${event.sizeBytes} bytes)`)
  })

  // Every failure at every severity arrives here, and this is the one event
  // type that cannot be switched off. `recovered` and `blocked` are the ones
  // worth watching: a failover that keeps working looks like success, and a
  // permission denial the agent quietly routed around looks like nothing at
  // all.
  thread.on('incident', (event) => {
    console.warn(`[${event.disposition}] ${event.code}: ${event.message}`)
  })

  // The wildcard. Fires for every event that reaches the client, in sequence
  // order, in addition to any typed handler above. Both run.
  //
  // Its real job is forward compatibility: event types are additive within a
  // proto major, so a newer satellite sends types this SDK version has no name
  // for. A typed handler cannot subscribe to a type it has never heard of.
  // This one gets them anyway, which makes it the correct hook for audit logs
  // and bus forwarding.
  thread.on('all', (event) => {
    auditLog.write({ sequence: event.sequence, type: event.type, event })
  })

  // Plans and questions answer back over HTTP, not up the socket, which is
  // unidirectional. Both live on the thread because only one plan and one
  // question set can ever be outstanding at a time.
  thread.on('plan.proposed', async (event) => {
    console.log(event.plan)
    await thread.approvePlan()
  })

  thread.on('question.asked', async (event) => {
    // The whole set is answered in one call. Individual answers may be an
    // option, freeform text, or a decline, but partial submission is not a
    // thing: all of them go back together.
    await thread.answerQuestions(
      event.questions.map((question): QuestionAnswer => {
        const preferredOption = question.options.find((option) => option.isRecommended)
        if (preferredOption) {
          return { questionId: question.id, optionId: preferredOption.id }
        }
        return { questionId: question.id, text: 'Use your best judgement.' }
      })
    )
  })
}


// ///////////////////////////// //
//  Style 2: the async iterator  //
// ///////////////////////////// //

/**
 * Pulls events off the same socket with an async iterator and a switch.
 *
 * The switch gives exhaustive narrowing on the event union, so adding a case is
 * the compiler's job to remind you about rather than yours to remember. In
 * exchange the loop body is serial: anything slow inside it stalls every event
 * behind it, and the satellite eventually closes the socket with
 * STREAM_CONSUMER_LAGGED. Reach for this when you actually want that
 * backpressure, or when you are resuming and want to drive the replay yourself.
 *
 * Pass `fromSequence` to pick up exactly where a previous consumer stopped.
 */
async function consumeEvents(thread: Thread, fromSequence?: number): Promise<void> {
  for await (const event of thread.events({ fromSequence })) {
    switch (event.type) {
      case 'agent.message':
        console.log(`[${event.author}] ${event.text}`)
        break

      case 'tool.started':
        console.log(`[${event.author}] ${event.toolName}`)
        break

      case 'team.member_spawned':
        console.log(`+ ${event.role} (${event.memberId})`)
        break

      case 'team.chat':
        console.log(`[team] ${event.author}: ${event.text}`)
        break

      case 'integration.landed':
        console.log(`merged ${event.memberId} into ${event.branch}`)
        break

      case 'integration.conflict':
        console.log(`conflict from ${event.memberId}, returned for resolution`)
        break

      case 'checker.result':
        console.log(`checker ${event.command} exited ${event.exitCode}`)
        break

      case 'budget.warning':
        console.warn(`budget at ${event.percentUsed}% of ${event.ceiling}`)
        break

      case 'plan.proposed':
        console.log(event.plan)
        await thread.approvePlan()
        break

      case 'question.asked':
        await thread.answerQuestions(
          event.questions.map((question): QuestionAnswer => {
            const preferredOption = question.options.find((option) => option.isRecommended)
            if (preferredOption) {
              return { questionId: question.id, optionId: preferredOption.id }
            }
            return { questionId: question.id, text: 'Use your best judgement.' }
          })
        )
        break

      case 'artifact.created':
        console.log(`artifact ${event.path} (${event.sizeBytes} bytes)`)
        break

      case 'incident':
        console.warn(`[${event.disposition}] ${event.code}: ${event.message}`)
        break

      case 'turn.completed':
        console.log(`turn finished: ${event.status}`)
        break

      // Codes and event types are additive within a proto major, so a newer
      // satellite can send something this SDK version has never heard of.
      // Log it, never throw on it.
      default:
        console.debug(`unhandled event type ${event.type}`, event)
    }
  }
}


// ///////////////////////////// //
//           Execution           //
// ///////////////////////////// //

async function main(): Promise<void> {
  // Create returns an object so the shape can grow without breaking callers.
  const { thread } = await satellite.threads.create(settings)
  console.log(`Thread ${thread.id} created`)

  attachHandlers(thread)

  const { turn } = await thread.startTurn({
    prompt: 'Add per-endpoint rate limiting to the public API and open a PR against develop.'
  })

  // Resolves when this turn reaches a terminal state. Handlers keep firing the
  // whole time.
  const result = await turn.result()
  console.log(result.summary)
  console.log(`${result.tokens.total} tokens, $${result.cost.toFixed(2)}`)

  // Counts by disposition ride along on the report, so the common case needs
  // no query at all. Query when you want the detail.
  console.log(result.incidentCounts)

  const problems = await thread.incidents.list({
    disposition: [ 'fatal', 'degraded' ],
    turnId: turn.id
  })
  for (const incident of problems) {
    console.warn(`${incident.code} (${incident.disposition}): ${incident.message}`)
  }

  // `fingerprint` is the field that makes this an issue pipeline rather than a
  // report. Suppress the ones you have already filed or you will open the same
  // ticket again on every turn.
  for (const suggestion of result.suggestions.techDebt) {
    if (alreadyFiled.has(suggestion.fingerprint)) {
      continue
    }
    await openIssue(suggestion.title, suggestion.body)
    alreadyFiled.add(suggestion.fingerprint)
  }

  // The agent's proposed setup script is inert data. Arsox never adopts it,
  // never writes it, never runs it. Adopting it is this line, and it is yours.
  const setup = result.suggestions.setupScript
  if (setup?.proposedSetupCommands) {
    console.log(`proposed setup change:\n${setup.proposedSetupCommands}`)
  }

  for (const artifact of await thread.artifacts.list()) {
    await thread.artifacts.download(artifact.path, `./out/${artifact.name}`)
  }

  // Or leave it to expire through the idle TTL. Incidents survive either way,
  // on their own retention, because "why did last night go wrong" is asked
  // after the workspace is gone.
  await thread.destroy()
}


/**
 * Picks a thread back up from a different process.
 *
 * A thread lives entirely on the satellite, so any process holding the URL, the
 * secret, and the thread ID can attach. This is how a horizontally scaled host
 * application survives a replica dying mid-turn: persist the thread ID and the
 * last sequence you saw, and whichever replica comes up next resumes from there
 * without losing an event.
 */
export async function resume(threadId: string, lastSeenSequence: number): Promise<void> {
  const { thread } = await satellite.threads.attach(threadId)
  await consumeEvents(thread, lastSeenSequence)
}

main().catch((error) => {
  console.error('Arsox run failed', error)
  process.exit(1)
})
