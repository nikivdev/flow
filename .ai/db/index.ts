// .ai/db/index.ts
// Database connection and utilities
import { drizzle } from "drizzle-orm/bun-sqlite"
import { Database } from "bun:sqlite"
import * as schema from "./schema"

const sqlite = new Database(".ai/db/db.sqlite")
export const db = drizzle(sqlite, { schema })

// Re-export schema for convenience
export * from "./schema"

// Helper to generate IDs
export const genId = () => crypto.randomUUID()

// Helper to get current timestamp
export const now = () => new Date()
