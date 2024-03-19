import { createContext } from "@/server/trpc";
import { OpenAI } from "openai";
import type { NextApiRequest, NextApiResponse } from "next";

type ResponseData = {
  status: string;
};

type ParsedData = {
  event: string;
  value: any;
};

const openai = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
});

export default async function handler(
  req: NextApiRequest,
  res: NextApiResponse<ResponseData>,
) {
  if (req.method === "POST") {
    const parsed = parse(req.body);
    const ctx = await createContext({ req, res });

    const response = await openai.chat.completions
      .create({
        model: "gpt-4",
        messages: [
          { role: "system", content: "You are a helpful assistant." },
          { role: "user", content: parsed.value.post.content },
        ],
        max_tokens: 150,
        n: 1,
        stop: null,
        temperature: 0.8,
      })
      .asResponse();

    const text = ((await response.json()) as any)["choices"][0]["message"][
      "content"
    ];

    const post = await ctx.prisma.post.create({
      data: {
        persona: {
          connect: {
            id: parsed.value.post.personaId,
          },
        },
        content: text,
        board: {
          connect: {
            title: "test",
          },
        },
      },
    });
  }

  res.status(200).json({ status: "ok" });
}

function parse(str: string): ParsedData {
  return JSON.parse(str);
}
