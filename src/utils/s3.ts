// lib/s3.js
import AWS from "aws-sdk";

const s3 = new AWS.S3({
  endpoint: "/api/s3-upload", // This is the endpoint for the local server, not the actual S3 endpoint. This is the key to making it work locally.
  accessKeyId: process.env.S3_UPLOAD_KEY,
  secretAccessKey: process.env.S3_UPLOAD_SECRET,
  s3ForcePathStyle: true,
  signatureVersion: "v4",
  region: process.env.S3_UPLOAD_REGION,
});

export default s3;
