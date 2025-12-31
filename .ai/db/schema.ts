// .ai/db/schema.ts
// Database schema for AI project data using drizzle-orm
import { sqliteTable, text, integer, blob } from "drizzle-orm/sqlite-core"

// Research notes and findings
export const research = sqliteTable("research", {
  id: text("id").primaryKey(),
  title: text("title").notNull(),
  content: text("content").notNull(),
  source: text("source"), // URL, file path, or reference
  tags: text("tags"), // JSON array of tags
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
  updatedAt: integer("updated_at", { mode: "timestamp" }),
})

// Tasks and todos tracked by agents
export const tasks = sqliteTable("tasks", {
  id: text("id").primaryKey(),
  title: text("title").notNull(),
  description: text("description"),
  status: text("status").notNull().default("pending"), // pending, in_progress, completed, blocked
  priority: integer("priority").default(0),
  parentId: text("parent_id"), // for subtasks
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
  completedAt: integer("completed_at", { mode: "timestamp" }),
})

// Files being tracked or generated
export const files = sqliteTable("files", {
  id: text("id").primaryKey(),
  path: text("path").notNull().unique(),
  contentHash: text("content_hash"),
  description: text("description"),
  generatedBy: text("generated_by"), // agent/tool that created it
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
  updatedAt: integer("updated_at", { mode: "timestamp" }),
})

// Key-value store for agent memory/state
export const memory = sqliteTable("memory", {
  key: text("key").primaryKey(),
  value: text("value").notNull(), // JSON serialized
  expiresAt: integer("expires_at", { mode: "timestamp" }),
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
})

// External service connections and context
export const connections = sqliteTable("connections", {
  id: text("id").primaryKey(),
  service: text("service").notNull(), // github, x, linear, etc.
  accountId: text("account_id"),
  metadata: text("metadata"), // JSON with service-specific data
  syncedAt: integer("synced_at", { mode: "timestamp" }),
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
})
