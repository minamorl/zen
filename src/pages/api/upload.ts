// pages/api/upload.ts

import type { NextApiRequest, NextApiResponse } from "next";
import { IncomingForm, Files, Fields } from "formidable";
import { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import { v4 as uuidv4 } from "uuid";
import fs from "fs";

const s3Client = new S3Client({
  endpoint: process.env.S3_UPLOAD_ENDPOINT,
  region: process.env.S3_UPLOAD_REGION,
  credentials: {
    accessKeyId: process.env.S3_UPLOAD_KEY!,
    secretAccessKey: process.env.S3_UPLOAD_SECRET!,
  },
  forcePathStyle: true,
});

export const config = {
  api: {
    bodyParser: false,
  },
};

export default async function handler(
  req: NextApiRequest,
  res: NextApiResponse,
) {
  if (req.method === "POST") {
    const form = new IncomingForm();

    form.parse(req, async (err, fields: Fields, files: Files) => {
      if (err) {
        console.error("Error parsing form data:", err);
        res.status(500).json({ error: "Error parsing form data" });
        return;
      }

      const file = Array.isArray(files.file) ? files.file[0] : files.file;

      if (!file) {
        res.status(400).json({ error: "No file provided" });
        return;
      }

      try {
        const fileStream = fs.createReadStream(file.filepath);

        // random file name via uuid
        const fileName = `${uuidv4()}-${file.originalFilename}`;
        const params = {
          Bucket: process.env.S3_UPLOAD_BUCKET!,
          Key: fileName,
          Body: fileStream,
          ContentType: file.mimetype!,
        };

        const command = new PutObjectCommand(params);
        await s3Client.send(command);

        const url = await getSignedUrl(s3Client, command, { expiresIn: 3600 });

        res.status(200).json({ url: fileName });
      } catch (error) {
        console.error("Error uploading file:", error);
        res.status(500).json({ error: "Error uploading file" });
      }
    });
  } else {
    res.status(405).json({ error: "Method not allowed" });
  }
}
