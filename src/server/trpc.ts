import { initTRPC } from "@trpc/server";
import { CreateNextContextOptions } from "@trpc/server/adapters/next";

import { PrismaClient } from "@prisma/client";
import { lucia } from "lucia";
import { prisma } from "@lucia-auth/adapter-prisma";

const prismaClient = new PrismaClient();

const auth = lucia({
  adapter: prisma(prismaClient, {
    user: "user",
    key: "userKey",
    session: "userSession",
  }),
  env: "DEV",
});

export const createContext = async ({ req, res }: CreateNextContextOptions) => {
  // Use the singleton Prisma client and Lucia auth in the context
  return {
    prisma: prismaClient,
    auth,
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
