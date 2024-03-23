import { createContext } from "@/server/trpc";
import type { NextApiRequest, NextApiResponse } from "next";

type ResponseData = {
  status: string;
};
type ParsedData = {
  event: string;
  value: any;
};

export default async function handler(
  req: NextApiRequest,
  res: NextApiResponse<ResponseData>,
) {
  if (req.method === "POST") {
    const parsed = parse(req.body);
    const ctx = await createContext({ req, res });
    if (parsed.value.post.content.startsWith("/cleanup")) {
      await ctx.prisma.post.deleteMany({
        // delete all posts
        where: {},
      });
    }
  }
  res.status(200).json({ status: "ok" });
}

function parse(str: string): ParsedData {
  return JSON.parse(str);
}
