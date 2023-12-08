import * as trpcNext from "@trpc/server/adapters/next";
import { z } from "zod";
import { procedure, router } from "../../../server/trpc";
import { createContext } from "../../../server/trpc";
import { v4 as uuidv4 } from "uuid";
import { LuciaError } from "lucia";
import { TRPCError } from "@trpc/server";
import { S3 } from "aws-sdk";
// Body
export const appRouter = router({
  getPresignedUrl: procedure
    .input(
      z.object({
        filename: z.string(),
        filetype: z.string(),
      }),
    )
    .mutation(async (opts) => {
      // s3 presigned url
      const s3 = new S3({
        accessKeyId: process.env.AWS_ACCESS_KEY_ID,
        secretAccessKey: process.env.AWS_SECRET_ACCESS_KEY,
        endpoint: "s3.ap-northeast-1.wasabisys.com", // Use the appropriate Wasabi endpoint
        region: "ap-northeast-1", // Use the appropriate Wasabi region
      });
      console.log(process.env.AWS_ACCESS_KEY_ID);
      console.log(process.env.AWS_SECRET_ACCESS_KEY);
      const key = uuidv4();
      const presignedUrl = await s3.getSignedUrlPromise("putObject", {
        Bucket: process.env.AWS_BUCKET_NAME,
        Key: key,
        Expires: 60 * 5, // 5 minutes
        ContentType: opts.input.filetype,
        ACL: "public-read",
      });

      return {
        presignedUrl,
        fileKey: key,
      };
    }),
  getBoard: procedure
    .input(
      z.object({
        name: z.string(),
      }),
    )
    .query(async (opts) => {
      // find or create board, order by created_at desc
      const board = await opts.ctx.prisma.board.findUnique({
        where: {
          title: opts.input.name,
        },
        include: {
          posts: {
            orderBy: {
              createdAt: "desc",
            },
            include: {
              threads: true,
              persona: true,
              resources: true,
            },
          },
        },
      });
      console.log(board);
      return board;
    }),
  createBoard: procedure
    .input(
      z.object({
        name: z.string(),
      }),
    )
    .mutation(async (opts) => {
      // create board
      const board = await opts.ctx.prisma.board.create({
        data: {
          title: opts.input.name,
        },
      });
      return board;
    }),
  createPost: procedure
    .input(
      z.object({
        persona_id: z.string(),
        raw_text: z.string(),
        board_name: z.string(),
        resource_key: z.string().optional(),
      }),
    )
    .mutation(async (opts) => {
      // create posts
      const post = await opts.ctx.prisma.post.create({
        data: {
          persona: {
            connect: {
              id: opts.input.persona_id,
            },
          },
          resources: opts.input.resource_key
            ? {
                create: {
                  imagePath: opts.input.resource_key,
                },
              }
            : undefined,
          content: opts.input.raw_text,
          board: {
            connect: {
              title: opts.input.board_name,
            },
          },
        },
      });
      console.log(post);
      return post;
    }),
  getPersonas: procedure.query(async (opts) => {
    // find auth user from session
    const user = opts.ctx.session.user;
    if (!user.userId) throw new Error("User not found");
    // find personas from user
    const personas = await opts.ctx.prisma.persona.findMany({
      where: {
        user: {
          id: user.userId,
        },
      },
    });
    return personas;
  }),

  signUp: procedure
    .input(
      z.object({
        email: z.string().email(),
        password: z.string().min(8),
      }),
    )
    .mutation(async ({ ctx, input }) => {
      try {
        console.log("signing up");
        const user = await ctx.auth.createUser({
          key: {
            providerId: "email",
            providerUserId: input.email,
            password: input.password,
          },
          attributes: {
            email: input.email,
          },
        });
        console.log(user);
        const session = await ctx.auth.createSession({
          userId: user.userId,
          attributes: {},
        });
        console.log(user, session);
        // set session sessionCookie
        const sessionCookie = ctx.auth.createSessionCookie(session);
        ctx.res.setHeader("Set-Cookie", sessionCookie.serialize());
        console.log(sessionCookie.serialize());
        return {
          message: "User created successfully",
          sessionId: session.sessionId,
          sessionCookie: sessionCookie.serialize(),
        };
      } catch (e) {
        console.error(e);
      }
    }),

  signIn: procedure
    .input(
      z.object({
        email: z.string().email(),
        password: z.string().min(8),
      }),
    )
    .mutation(async ({ ctx, input }) => {
      try {
        // Normalize username to lowercase to handle case sensitivity
        const email = input.email.toLowerCase();
        const password = input.password;

        // Authenticate user and create a session
        const key = await ctx.auth.useKey("email", email, password);
        const session = await ctx.auth.createSession({
          userId: key.userId,
          attributes: {},
        });
        const sessionCookie = ctx.auth.createSessionCookie(session);
        ctx.res.setHeader("Set-Cookie", sessionCookie.serialize());

        // Return session information and cookie for client to store
        return {
          message: "Authentication successful",
          sessionId: session.sessionId,
          sessionCookie: sessionCookie.serialize(),
        };
      } catch (e) {
        if (
          e instanceof LuciaError &&
          (e.message === "AUTH_INVALID_KEY_ID" ||
            e.message === "AUTH_INVALID_PASSWORD")
        ) {
          throw new TRPCError({
            code: "UNAUTHORIZED",
            message: "Incorrect username or password",
          });
        }
        throw new TRPCError({
          code: "INTERNAL_SERVER_ERROR",
          message: "An unknown error occurred",
        });
      }
    }),

  createThread: procedure
    .input(
      z.object({
        post_id: z.string(),
        raw_text: z.string(),
        persona_id: z.string(),
      }),
    )
    .mutation(async (opts) => {
      return await opts.ctx.prisma.thread.create({
        data: {
          post: {
            connect: {
              id: opts.input.post_id,
            },
          },
          persona: {
            connect: {
              id: opts.input.persona_id,
            },
          },
          content: opts.input.raw_text,
        },
      });
    }),
  createPersona: procedure
    .input(
      z.object({
        name: z.string(),
      }),
    )
    .mutation(async (opts) => {
      // create persona
      const persona = await opts.ctx.prisma.persona.create({
        data: {
          name: opts.input.name,
          user: {
            connect: {
              id: opts.ctx.session.user.userId,
            },
          },
        },
      });
      // return personas
      return persona;
    }),
});
export type AppRouter = typeof appRouter;
// export API handler
// @see https://trpc.io/docs/server/adapters
export default trpcNext.createNextApiHandler({
  router: appRouter,
  createContext,
});
