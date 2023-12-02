import * as trpcNext from "@trpc/server/adapters/next";
import { z } from "zod";
import { procedure, router } from "../../../server/trpc";
import { createContext } from "../../../server/trpc";
import { v4 as uuidv4 } from "uuid";
// Body
export const appRouter = router({
  // upload: procedure
  //   .input(
  //     z.object({
  //       file: z.any(),
  //       persona_id: z.string(),
  //     }),
  //   )
  //   .mutation(async (opts) => {
  //     const filename = uuidv4();
  //
  //     console.log(opts.input.file.type);
  //     const { data, error } = await opts.ctx.supabase.storage
  //       .from("images")
  //       .upload(filename, opts.input.file, {
  //         contentType: opts.input.file.type,
  //       });
  //     console.log(data, error);
  //     const { data: resource, error: resourceError } = await opts.ctx.supabase
  //       .from("resources")
  //       .insert({
  //         path: filename,
  //         persona_id: opts.input.persona_id,
  //       })
  //       .select();
  //     return resource;
  //   }),
  getBoard: procedure
    .input(
      z.object({
        name: z.string(),
      }),
    )
    .query(async (opts) => {
      const { data } = await opts.ctx.supabase
        .from("boards")
        .select(
          `
                                                                  id,
                                                                  created_at,
                                                                  name,
                                                                  description,
                                                                  posts (
                                                                    id,
                                                                    raw_text,
                                                                    persona_id,
                                                                    created_at,
                                                                    threads (
                                                                      id,
                                                                      raw_text,
                                                                      persona_id,
                                                                      created_at
                                                                    ),
                                                                    personas(
                                                                      id,
                                                                      name,
                                                                      resources(
                                                                        id,
                                                                        path
                                                                      )
                                                                    )
                                                                    
                                                                  )`,
        )
        .eq("name", opts.input.name);

      if (!data) return null;
      const board = data[0];
      // sort by created_at desc
      board.posts.sort((a, b) => {
        if (a.created_at < b.created_at) return 1;
        if (a.created_at > b.created_at) return -1;
        return 0;
      });
      return board;
    }),
  getPosts: procedure.query(async (opts) => {
    const { data } = await opts.ctx.supabase.from("posts").select(`
                                                                  id,
                                                                  raw_text,
                                                                  persona_id,
                                                                  created_at,
                                                                  threads (
                                                                    id,
                                                                    raw_text,
                                                                    persona_id,
                                                                    created_at
                                                                  ),
                                                                  `);
    return data;
  }),
  createResource: procedure
    .input(
      z.object({
        path: z.string(),
        post_id: z.string(),
      }),
    )
    .mutation(async (opts) => {
      return await opts.ctx.supabase
        .from("resources")
        .insert({
          path: opts.input.path,
          post_id: opts.input.post_id,
        })
        .select();
    }),
  createPost: procedure
    .input(
      z.object({
        persona_id: z.string(),
        raw_text: z.string(),
      }),
    )
    .mutation(async (opts) => {
      if (!opts.ctx.user.user) throw new Error("Post creation failed");
      const personas = await opts.ctx.supabase
        .from("personas")
        .select()
        .eq("user_id", opts.ctx.user.user.id)
        .eq("id", opts.input.persona_id);
      if (personas.count && personas.count < 1)
        throw new Error("Persona not found");
      const { data, error } = await opts.ctx.supabase
        .from("posts")
        .insert({
          raw_text: opts.input.raw_text,
          persona_id: opts.input.persona_id,
          board_id: "d7946875-8f2e-434d-90ec-b08524dd5303",
        })
        .select();

      return data;
    }),
  getPersonas: procedure.query(async (opts) => {
    const { data: user, error } = await opts.ctx.supabase.auth.getUser(
      opts.ctx.req.cookies.token,
    );
    if (!user.user) throw new Error("User not found");
    const personas = await opts.ctx.supabase
      .from("personas")
      .select()
      .eq("user_id", user.user.id);
    return {
      user: user.user,
      personas: personas.data,
    };
  }),
  signIn: procedure
    .input(
      z.object({
        email: z.string(),
        password: z.string(),
      }),
    )
    .mutation(async (opts) => {
      const { data: user } = await opts.ctx.supabase.auth.signInWithPassword({
        email: opts.input.email,
        password: opts.input.password,
      });
      if (!user.user) {
        throw new Error();
      }
      opts.ctx.res.setHeader(
        "set-cookie",
        "token=" + user.session.access_token,
      );

      const personas = await opts.ctx.supabase
        .from("personas")
        .select()
        .eq("user_id", user.user.id);
      return {
        user,
        personas,
      };
    }),

  createUser: procedure
    .input(
      z.object({
        email: z.string(),
        password: z.string(),
      }),
    )
    .query(async (opts) => {
      const { data, error } = await opts.ctx.supabase.auth.signUp({
        email: opts.input.email,
        password: opts.input.password,
      });
      if (error) return error;
      return data;
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
      const { data, error } = await opts.ctx.supabase.from("threads").insert({
        post_id: opts.input.post_id,
        raw_text: opts.input.raw_text,
        persona_id: opts.input.persona_id,
      });
      return data;
    }),
  createPersona: procedure
    .input(
      z.object({
        name: z.string(),
      }),
    )
    .mutation(async (opts) => {
      if (!opts.ctx.user.user) throw new Error("Persona creation failed");
      const { data, error } = await opts.ctx.supabase
        .from("personas")
        .insert({
          name: opts.input.name,
          user_id: opts.ctx.user.user.id,
        })
        .select();
      console.log(data, error);
      return data;
    }),
});
export type AppRouter = typeof appRouter;
// export API handler
// @see https://trpc.io/docs/server/adapters
export default trpcNext.createNextApiHandler({
  router: appRouter,
  createContext,
});
