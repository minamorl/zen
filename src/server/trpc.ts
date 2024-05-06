import { initTRPC } from "@trpc/server";
import { CreateNextContextOptions } from "@trpc/server/adapters/next";

import { PrismaClient } from "@prisma/client";
import { lucia } from "lucia";
import { prisma } from "@lucia-auth/adapter-prisma";
import { web } from "lucia/middleware";

const prismaClient = new PrismaClient();

const auth = lucia({
  adapter: prisma(prismaClient, {
    user: "user",
    key: "key",
    session: "session",
  }),
  env: "DEV",
  middleware: web(),
  sessionCookie: {
    expires: false,
  },
});

export const createContext = async ({ req, res }: CreateNextContextOptions) => {
  const sessionId = auth.readSessionCookie(req.headers.cookie)!;
  let session = null;
  try {
    session = await auth.getSession(sessionId);
  } catch (e) {
    // Session is wrong. clear the cookie
    console.error("Session is wrong. clear the cookie");
    res.setHeader("Set-Cookie", "");
  }

  // Use the singleton Prisma client and Lucia auth in the context
  return {
    prisma: prismaClient,
    auth,
    session,
    req,
    res,
  };
};
// Avoid exporting the entire t-object
// since it's not very descriptive.
// For instance, the use of a t variable
// is common in i18n libraries.
const t = initTRPC.context<typeof createContext>().create();
// Base router and procedure helpers
export const router = t.router;
export const procedure = t.procedure;
